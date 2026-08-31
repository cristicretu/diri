//! Idempotent remote Helper bootstrap and management RPCs.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_proto::HostEntry;
use diri_proto::remote_pty::{
    DirectoryListRequest, DirectoryListResult, EnvironmentCaptureRequest, EnvironmentCaptureResult,
    ExecutableDiscoveryRequest, ExecutableDiscoveryResult, GcResult, HelperProbe, LaunchRequest,
    LaunchResult, PHASE_ONE_HELPER_CAPABILITIES, PersistenceCapability, PersistenceProbeAction,
    PersistenceProbeRequest, PersistenceProbeResult, ProtocolVersion, SessionInspection,
    SessionSelector,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::bootstrap::{PackagedArtifact, PlatformProbe, RemoteInstallLayout, RemoteTarget};
use super::executor::{CommandOutput, ProcessExecutor, SshChannel};
use super::ssh::{HelperCommand, SshTransport};

// These wall-clock bounds include time spent in Diri's native OpenSSH
// authentication UI. Network establishment is independently bounded by
// `ConnectTimeout`, and Helper-side environment capture has its own shorter
// deadline, so allowing a human to unlock a key cannot create an unbounded
// remote command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const RPC_TIMEOUT: Duration = Duration::from_secs(120);
const PERSISTENCE_LOGOUT_SETTLE: Duration = Duration::from_secs(1);
// Environment capture alone is bounded at 1 MiB; executable discovery adds a
// bounded result row per query to that same response.
const MAX_RPC_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
// macOS sockaddr_un.sun_path is 104 bytes. OpenSSH briefly appends a dot and
// 16-byte nonce while creating a multiplex socket, so leave room for the
// per-host digest plus that suffix.
const MAX_CONTROL_DIRECTORY_BYTES: usize = 56;
fn verify_required_helper_probe(
    artifact: &PackagedArtifact,
    probe: &HelperProbe,
) -> io::Result<()> {
    artifact.verify_probe(probe).map_err(io::Error::other)?;
    if let Some(missing) = PHASE_ONE_HELPER_CAPABILITIES
        .iter()
        .find(|capability| !probe.capabilities.contains(capability))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "remote Helper is missing required capability {}",
                missing.wire_name()
            ),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ArtifactCatalog {
    artifacts: HashMap<RemoteTarget, PackagedArtifact>,
}

impl ArtifactCatalog {
    pub fn from_manifest(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let manifest: ArtifactManifest = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "artifact manifest has no parent",
            )
        })?;
        let mut artifacts = HashMap::new();
        for entry in manifest.artifacts {
            let target =
                RemoteTarget::from_artifact_name(&entry.target).map_err(io::Error::other)?;
            let relative = Path::new(&entry.path);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "artifact path must be normalized and relative to its manifest",
                ));
            }
            let artifact = PackagedArtifact {
                target,
                protocol_major: manifest.protocol_major,
                build_id: manifest.build_id.clone(),
                length: entry.length,
                sha256: entry.sha256,
                path: parent.join(relative),
            };
            artifact.verify().map_err(io::Error::other)?;
            if artifacts.insert(target, artifact).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "artifact manifest contains a duplicate target",
                ));
            }
        }
        for target in RemoteTarget::ALL {
            if !artifacts.contains_key(&target) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "artifact manifest is missing required target {}",
                        target.artifact_name()
                    ),
                ));
            }
        }
        Ok(Self { artifacts })
    }

    /// Development and deterministic-test catalog containing the native
    /// Helper binary only. Release builds use the complete supported-target
    /// manifest.
    pub fn from_native_helper(path: &Path) -> io::Result<Self> {
        let output = std::process::Command::new(path)
            .args(["probe", "--format=json"])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("native Helper probe failed"));
        }
        let probe: HelperProbe = serde_json::from_slice(&output.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let target = RemoteTarget::from_artifact_name(&probe.target).map_err(io::Error::other)?;
        let artifact = PackagedArtifact {
            target,
            protocol_major: probe.protocol.major,
            build_id: probe.build_id,
            length: fs::metadata(path)?.len(),
            sha256: probe.artifact_sha256,
            path: path.to_path_buf(),
        };
        artifact.verify().map_err(io::Error::other)?;
        Ok(Self {
            artifacts: HashMap::from([(target, artifact)]),
        })
    }

    fn artifact(&self, target: RemoteTarget) -> io::Result<&PackagedArtifact> {
        self.artifacts.get(&target).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("no packaged Helper for {}", target.artifact_name()),
            )
        })
    }
}

#[derive(Clone, Debug)]
pub struct RemoteManager {
    executor: ProcessExecutor,
    artifacts: ArtifactCatalog,
    control_dir: PathBuf,
    persistence: Arc<Mutex<HashMap<String, PersistenceCapability>>>,
    /// Process-local target discovery cache. Every new remote action still
    /// probes the exact packaged Build ID before use; this cache only removes
    /// the redundant uname/platform round trip.
    current_helpers: Arc<Mutex<HashMap<String, CurrentHelper>>>,
}

#[derive(Clone, Debug)]
struct CurrentHelper {
    target: RemoteTarget,
    helper: InstalledHelper,
}

impl RemoteManager {
    pub fn new(
        executor: ProcessExecutor,
        artifacts: ArtifactCatalog,
        control_dir: PathBuf,
    ) -> io::Result<Self> {
        let control_dir = normalized_control_dir(&control_dir);
        validate_control_dir_if_present(&control_dir)?;
        fs::create_dir_all(&control_dir)?;
        // Recheck after creation to close the `/tmp` create race before chmod
        // or any OpenSSH socket creation follows an attacker-placed symlink.
        validate_control_dir_if_present(&control_dir)?;
        fs::set_permissions(&control_dir, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            executor,
            artifacts,
            control_dir,
            persistence: Arc::new(Mutex::new(HashMap::new())),
            current_helpers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Ensures this Engine build's exact Helper is installed and valid. A
    /// warm host needs one multiplexed `probe` round trip; a version change or
    /// failed probe falls through to the full platform-select/install path.
    pub fn ensure_helper(&self, host: &HostEntry) -> io::Result<InstalledHelper> {
        if let Some(helper) = self.verify_cached_current(host)? {
            return Ok(helper);
        }
        self.bootstrap_helper(host, false)
    }

    /// Forces the packaged bytes through upload, temporary verification and
    /// activation. Activation is content-addressed: an existing identical
    /// build is retained, while an incompatible occupant is never replaced.
    pub fn reinstall_helper(&self, host: &HostEntry) -> io::Result<InstalledHelper> {
        self.current_helpers
            .lock()
            .expect("current Helper cache")
            .remove(&host.ssh);
        self.persistence
            .lock()
            .expect("persistence cache")
            .remove(&persistence_key(host));
        self.bootstrap_helper(host, true)
    }

    /// Closes finite-lived OpenSSH multiplexers after the owning Engine has
    /// become fully idle. Live remote sessions never call this path.
    pub fn close_control_masters(&self) -> io::Result<()> {
        let transports = self
            .current_helpers
            .lock()
            .expect("current Helper cache")
            .values()
            .map(|current| current.helper.transport.clone())
            .collect::<Vec<_>>();
        let mut result = Ok(());
        for transport in transports {
            if let Err(error) = self.close_control_master(&transport)
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }

    /// Ends Diri's shared SSH connection and waits for its local control
    /// socket to disappear. A missing socket means there is no master to hide
    /// logout cleanup; any other failed teardown is a correctness error for a
    /// persistence probe.
    fn close_control_master(&self, transport: &SshTransport) -> io::Result<()> {
        let output = self.executor.run(
            transport.control_exit(),
            Vec::new(),
            Duration::from_secs(2),
            4 * 1024,
        )?;
        if !output.status.success() && transport.control_path().exists() {
            return Err(io::Error::other(format!(
                "SSH control-master teardown failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while transport.control_path().exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if transport.control_path().exists() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH control master did not close before the persistence probe",
            ));
        }
        Ok(())
    }

    fn verify_cached_current(&self, host: &HostEntry) -> io::Result<Option<InstalledHelper>> {
        let cached = self
            .current_helpers
            .lock()
            .expect("current Helper cache")
            .get(&host.ssh)
            .cloned();
        let Some(cached) = cached else {
            return Ok(None);
        };
        let artifact = self.artifacts.artifact(cached.target)?;
        artifact.verify().map_err(io::Error::other)?;
        if artifact.build_id != cached.helper.build_id {
            self.forget_current_helper(host);
            return Ok(None);
        }
        let output = self.executor.run(
            cached
                .helper
                .transport
                .helper_probe(&artifact.build_id)
                .map_err(io::Error::other)?,
            Vec::new(),
            PROBE_TIMEOUT,
            MAX_RPC_OUTPUT,
        )?;
        if !output.status.success() || output.stdout_truncated {
            self.forget_current_helper(host);
            return Ok(None);
        }
        let probe = match parse_json_line::<HelperProbe>(&output.stdout) {
            Ok(probe) => probe,
            Err(_) => {
                self.forget_current_helper(host);
                return Ok(None);
            }
        };
        if verify_required_helper_probe(artifact, &probe).is_err() || !probe.holder_available {
            self.forget_current_helper(host);
            return Ok(None);
        }
        let helper = InstalledHelper {
            build_id: artifact.build_id.clone(),
            protocol: probe.protocol,
            transport: cached.helper.transport,
        };
        self.remember_current_helper(host, cached.target, &helper);
        Ok(Some(helper))
    }

    fn bootstrap_helper(
        &self,
        host: &HostEntry,
        force_upload: bool,
    ) -> io::Result<InstalledHelper> {
        let transport = self.transport(host);
        let platform = self
            .executor
            .run(
                transport.platform_probe(),
                Vec::new(),
                PROBE_TIMEOUT,
                MAX_RPC_OUTPUT,
            )?
            .require_success("remote platform probe")?;
        if platform.stdout_truncated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote platform probe output was truncated",
            ));
        }
        let platform = PlatformProbe::parse(&platform.stdout).map_err(io::Error::other)?;
        let artifact = self.artifacts.artifact(platform.target)?;
        artifact.verify().map_err(io::Error::other)?;

        if !force_upload {
            let final_probe = self.executor.run(
                transport
                    .helper_probe(&artifact.build_id)
                    .map_err(io::Error::other)?,
                Vec::new(),
                PROBE_TIMEOUT,
                MAX_RPC_OUTPUT,
            )?;
            if final_probe.status.success() {
                let probe = parse_json_line::<HelperProbe>(&final_probe.stdout)?;
                verify_required_helper_probe(artifact, &probe)?;
                if !probe.holder_available {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "remote Helper does not provide a Holder",
                    ));
                }
                let helper = InstalledHelper {
                    build_id: artifact.build_id.clone(),
                    protocol: probe.protocol,
                    transport,
                };
                self.remember_current_helper(host, platform.target, &helper);
                return Ok(helper);
            }
        }

        if artifact.length > MAX_ARTIFACT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "packaged Helper exceeds 64 MiB",
            ));
        }
        let nonce = random_hex(16)?;
        let layout =
            RemoteInstallLayout::new(&artifact.build_id, nonce).map_err(io::Error::other)?;
        let upload = fs::read(&artifact.path)?;
        let install: io::Result<HelperProbe> = (|| {
            self.executor
                .run(
                    transport.upload(&layout),
                    upload,
                    UPLOAD_TIMEOUT,
                    MAX_RPC_OUTPUT,
                )?
                .require_success("remote Helper upload")?;
            let temporary = self
                .executor
                .run(
                    transport.temporary_probe(&layout),
                    Vec::new(),
                    PROBE_TIMEOUT,
                    MAX_RPC_OUTPUT,
                )?
                .require_success("temporary Helper verification")?;
            let temporary = parse_json_line::<HelperProbe>(&temporary.stdout)?;
            verify_required_helper_probe(artifact, &temporary)?;
            let activated = self
                .executor
                .run(
                    transport.commit_upload(&layout),
                    Vec::new(),
                    PROBE_TIMEOUT,
                    MAX_RPC_OUTPUT,
                )?
                .require_success("Helper activation")?;
            let activated = parse_json_line::<HelperProbe>(&activated.stdout)?;
            verify_required_helper_probe(artifact, &activated)?;
            let final_probe = self
                .executor
                .run(
                    transport
                        .helper_probe(&artifact.build_id)
                        .map_err(io::Error::other)?,
                    Vec::new(),
                    PROBE_TIMEOUT,
                    MAX_RPC_OUTPUT,
                )?
                .require_success("activated Helper verification")?;
            let final_probe = parse_json_line::<HelperProbe>(&final_probe.stdout)?;
            verify_required_helper_probe(artifact, &final_probe)?;
            if !final_probe.holder_available {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "activated remote Helper does not provide a Holder",
                ));
            }
            Ok(final_probe)
        })();
        if install.is_err() {
            let _ = self.executor.run(
                transport.cleanup_upload(&layout),
                Vec::new(),
                PROBE_TIMEOUT,
                MAX_RPC_OUTPUT,
            );
        }
        let probe = install?;
        let helper = InstalledHelper {
            build_id: artifact.build_id.clone(),
            protocol: probe.protocol,
            transport,
        };
        self.remember_current_helper(host, platform.target, &helper);
        Ok(helper)
    }

    /// Lists one bounded directory level through the exact installed Helper.
    /// Version verification and the bounded listing each use one multiplexed
    /// round trip. A failed action clears the target cache and retries through
    /// the full verified bootstrap path once.
    pub fn list_directories(
        &self,
        host: &HostEntry,
        request: &DirectoryListRequest,
    ) -> io::Result<DirectoryListResult> {
        let helper = self.ensure_helper(host)?;
        match self.rpc(&helper, HelperCommand::Directories, request, RPC_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(_) => {
                self.forget_current_helper(host);
                let helper = self.ensure_helper(host)?;
                self.rpc(&helper, HelperCommand::Directories, request, RPC_TIMEOUT)
            }
        }
    }

    /// Resolves every manifest executable in one login-environment capture and
    /// one Helper RPC. A warm retry still verifies the exact packaged build.
    pub fn discover_executables(
        &self,
        host: &HostEntry,
        request: &ExecutableDiscoveryRequest,
    ) -> io::Result<ExecutableDiscoveryResult> {
        let helper = self.ensure_helper(host)?;
        match self.rpc(&helper, HelperCommand::Executables, request, RPC_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(_) => {
                self.forget_current_helper(host);
                let helper = self.ensure_helper(host)?;
                self.rpc(&helper, HelperCommand::Executables, request, RPC_TIMEOUT)
            }
        }
    }

    /// Executes an Engine-owned fixed POSIX script over the host's multiplexed
    /// no-PTY SSH transport. Dynamic values must travel in `input`, never in
    /// the command text. This seam is reserved for bounded host orchestration
    /// such as Git inspection; Agent argv never passes through it.
    pub(crate) fn run_fixed_script(
        &self,
        host: &HostEntry,
        script: &'static str,
        input: Vec<u8>,
        timeout: Duration,
        output_limit: usize,
    ) -> io::Result<CommandOutput> {
        // Fixed orchestration scripts do not execute through the Helper, but
        // they are still remote actions. Gate them on the exact packaged
        // Helper so an app update synchronizes the host before doing work.
        self.ensure_helper(host)?;
        self.executor.run(
            self.transport(host).channel(script),
            input,
            timeout,
            output_limit,
        )
    }

    fn remember_current_helper(
        &self,
        host: &HostEntry,
        target: RemoteTarget,
        helper: &InstalledHelper,
    ) {
        self.current_helpers
            .lock()
            .expect("current Helper cache")
            .insert(
                host.ssh.clone(),
                CurrentHelper {
                    target,
                    helper: helper.clone(),
                },
            );
    }

    fn forget_current_helper(&self, host: &HostEntry) {
        self.current_helpers
            .lock()
            .expect("current Helper cache")
            .remove(&host.ssh);
    }

    /// Reopens the exact version referenced by a live session. Old builds are
    /// intentionally allowed to coexist with the current packaged artifact.
    pub fn existing_helper(
        &self,
        host: &HostEntry,
        build_id: &str,
        protocol: ProtocolVersion,
    ) -> io::Result<InstalledHelper> {
        let transport = self.transport(host);
        let output = self
            .executor
            .run(
                transport.helper_probe(build_id).map_err(io::Error::other)?,
                Vec::new(),
                PROBE_TIMEOUT,
                MAX_RPC_OUTPUT,
            )?
            .require_success("existing remote Helper verification")?;
        let probe = parse_json_line::<HelperProbe>(&output.stdout)?;
        if probe.build_id != build_id
            || probe.protocol.major != protocol.major
            || !probe.holder_available
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing remote Helper identity or protocol does not match",
            ));
        }
        Ok(InstalledHelper {
            build_id: build_id.to_string(),
            protocol: probe.protocol,
            transport,
        })
    }

    pub fn capture_environment(
        &self,
        helper: &InstalledHelper,
        request: &EnvironmentCaptureRequest,
    ) -> io::Result<EnvironmentCaptureResult> {
        self.rpc(
            helper,
            HelperCommand::Environment,
            request,
            Duration::from_secs(12),
        )
    }

    /// Tests survival after one SSH connection closes and over a second,
    /// independently authenticated connection. The result is cached only for
    /// this Engine lifetime and never inferred from `setsid`.
    pub fn probe_persistence(
        &self,
        host: &HostEntry,
        helper: &InstalledHelper,
    ) -> io::Result<PersistenceCapability> {
        let key = persistence_key(host);
        if let Some(capability) = self
            .persistence
            .lock()
            .expect("persistence cache")
            .get(&key)
            .copied()
        {
            return Ok(capability);
        }

        // Bootstrap and ordinary RPCs may have left a finite-lived
        // ControlMaster behind. Close it before starting the witness, then use
        // only non-multiplexed persistence RPCs so the Begin command cannot
        // return until its underlying SSH connection is gone.
        self.close_control_master(&helper.transport)?;

        let native = self.probe_persistence_mode(helper, PersistenceProbeAction::BeginNative)?;
        let supervised = if native {
            false
        } else {
            self.probe_persistence_mode(helper, PersistenceProbeAction::BeginSupervisor)?
        };
        let capability = classify_persistence(native, supervised);
        self.persistence
            .lock()
            .expect("persistence cache")
            .insert(key, capability);
        Ok(capability)
    }

    fn probe_persistence_mode(
        &self,
        helper: &InstalledHelper,
        action: PersistenceProbeAction,
    ) -> io::Result<bool> {
        let nonce = random_hex(16)?;
        let begin: PersistenceProbeResult = self.rpc(
            helper,
            HelperCommand::Persistence,
            &PersistenceProbeRequest {
                nonce: nonce.clone(),
                action,
            },
            RPC_TIMEOUT,
        )?;
        let checked = if begin.alive {
            // The begin SSH command has exited before `rpc` returns. This
            // margin lets login-scope cleanup finish before a separate
            // channel checks the witness.
            std::thread::sleep(PERSISTENCE_LOGOUT_SETTLE);
            self.rpc::<_, PersistenceProbeResult>(
                helper,
                HelperCommand::Persistence,
                &PersistenceProbeRequest {
                    nonce: nonce.clone(),
                    action: PersistenceProbeAction::Check,
                },
                RPC_TIMEOUT,
            )?
        } else {
            PersistenceProbeResult { alive: false }
        };
        let cleanup = self.rpc::<_, PersistenceProbeResult>(
            helper,
            HelperCommand::Persistence,
            &PersistenceProbeRequest {
                nonce,
                action: PersistenceProbeAction::Cleanup,
            },
            RPC_TIMEOUT,
        );
        if let Err(error) = cleanup {
            return Err(io::Error::other(format!(
                "persistence probe cleanup failed: {error}"
            )));
        }
        Ok(checked.alive)
    }

    pub fn launch(
        &self,
        helper: &InstalledHelper,
        request: &LaunchRequest,
    ) -> io::Result<LaunchResult> {
        match self.rpc(helper, HelperCommand::Launch, request, UPLOAD_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(first) => {
                // Launch is authenticated and idempotent for a
                // session/token pair. A channel may fail after the Holder was
                // created but before its response arrived; one fresh SSH
                // command recovers that exact incarnation without spawning a
                // duplicate or leaking an unknown Holder.
                std::thread::sleep(Duration::from_millis(50));
                self.rpc(helper, HelperCommand::Launch, request, UPLOAD_TIMEOUT)
                    .map_err(|second| {
                        io::Error::new(
                            second.kind(),
                            format!(
                                "remote launch failed before and after idempotent retry: {first}; {second}"
                            ),
                        )
                    })
            }
        }
    }

    pub fn inspect(
        &self,
        helper: &InstalledHelper,
        selector: &SessionSelector,
    ) -> io::Result<SessionInspection> {
        self.rpc(helper, HelperCommand::Inspect, selector, RPC_TIMEOUT)
    }

    pub fn list(&self, helper: &InstalledHelper) -> io::Result<Vec<SessionInspection>> {
        self.rpc_empty(helper, HelperCommand::List, RPC_TIMEOUT)
    }

    pub fn kill(
        &self,
        helper: &InstalledHelper,
        selector: &SessionSelector,
    ) -> io::Result<SessionInspection> {
        self.rpc(helper, HelperCommand::Kill, selector, RPC_TIMEOUT)
    }

    pub fn gc(&self, helper: &InstalledHelper) -> io::Result<GcResult> {
        self.rpc_empty(helper, HelperCommand::Gc, RPC_TIMEOUT)
    }

    pub fn attach(&self, helper: &InstalledHelper) -> io::Result<SshChannel> {
        self.executor.open(
            helper
                .transport
                .helper_command(&helper.build_id, HelperCommand::Attach)
                .map_err(io::Error::other)?,
        )
    }

    fn rpc<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        helper: &InstalledHelper,
        command: HelperCommand,
        request: &T,
        timeout: Duration,
    ) -> io::Result<R> {
        let input = serde_json::to_vec(request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.rpc_input(helper, command, input, timeout)
    }

    fn rpc_empty<R: serde::de::DeserializeOwned>(
        &self,
        helper: &InstalledHelper,
        command: HelperCommand,
        timeout: Duration,
    ) -> io::Result<R> {
        self.rpc_input(helper, command, Vec::new(), timeout)
    }

    fn rpc_input<R: serde::de::DeserializeOwned>(
        &self,
        helper: &InstalledHelper,
        command: HelperCommand,
        input: Vec<u8>,
        timeout: Duration,
    ) -> io::Result<R> {
        let output = self
            .executor
            .run(
                helper
                    .transport
                    .helper_command(&helper.build_id, command)
                    .map_err(io::Error::other)?,
                input,
                timeout,
                MAX_RPC_OUTPUT,
            )?
            .require_success("remote Helper RPC")?;
        if output.stdout_truncated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote Helper response was truncated",
            ));
        }
        parse_json_line(&output.stdout)
    }

    fn transport(&self, host: &HostEntry) -> SshTransport {
        let digest = Sha256::digest(host.ssh.as_bytes());
        let name = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        SshTransport::new(host, self.control_dir.join(name))
            .with_executable(self.executor.ssh_executable().to_os_string())
    }
}

fn validate_control_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != effective_uid() =>
        {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SSH control directory must be a real directory owned by the current user",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn normalized_control_dir(requested: &Path) -> PathBuf {
    if requested.as_os_str().as_bytes().len() <= MAX_CONTROL_DIRECTORY_BYTES {
        return requested.to_path_buf();
    }
    let digest = Sha256::digest(requested.as_os_str().as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // `/tmp` is intentionally explicit: macOS's TMPDIR path is commonly long
    // enough to reproduce the same sockaddr_un failure. The sticky parent and
    // the owner/type checks in `new` protect this private child directory.
    PathBuf::from(format!("/tmp/diri-ssh-{}-{suffix}", effective_uid()))
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not access caller-owned
    // memory; it returns the kernel credential for this process.
    unsafe { libc::geteuid() }
}

#[derive(Clone, Debug)]
pub struct InstalledHelper {
    pub build_id: String,
    pub protocol: ProtocolVersion,
    pub transport: SshTransport,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactManifest {
    protocol_major: u16,
    build_id: String,
    artifacts: Vec<ArtifactEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactEntry {
    target: String,
    path: String,
    length: u64,
    sha256: String,
}

fn parse_json_line<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    for line in bytes.split(|byte| *byte == b'\n').rev() {
        let line = line
            .iter()
            .copied()
            .skip_while(u8::is_ascii_whitespace)
            .collect::<Vec<_>>();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_slice(&line) {
            return Ok(value);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Helper JSON response is missing or invalid",
    ))
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random)
        .map_err(|error| io::Error::other(format!("secure random source failed: {error}")))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn classify_persistence(native: bool, supervised: bool) -> PersistenceCapability {
    if native {
        PersistenceCapability::NativeDetach
    } else if supervised {
        PersistenceCapability::UserSupervisor
    } else {
        PersistenceCapability::NonPersistent
    }
}

fn persistence_key(host: &HostEntry) -> String {
    format!("{}\0{}", host.id, host.ssh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    #[test]
    fn json_line_parser_ignores_bounded_shell_noise() {
        let parsed: HelperProbe = parse_json_line(
            b"welcome from rc\n{\"protocol\":{\"major\":1,\"minor\":1},\"buildId\":\"b\",\"artifactSha256\":\"h\",\"target\":\"t\",\"os\":\"o\",\"arch\":\"a\",\"supported\":true,\"holderAvailable\":true,\"capabilities\":[]}\n",
        )
        .expect("parse");
        assert_eq!(parsed.build_id, "b");
    }

    #[test]
    fn json_line_parser_ignores_trailing_shell_noise() {
        let parsed: HelperProbe = parse_json_line(
            b"{\"protocol\":{\"major\":1,\"minor\":1},\"buildId\":\"b\",\"artifactSha256\":\"h\",\"target\":\"t\",\"os\":\"o\",\"arch\":\"a\",\"supported\":true,\"holderAvailable\":true,\"capabilities\":[]}\nlogout noise\n",
        )
        .expect("parse");
        assert_eq!(parsed.build_id, "b");
    }

    #[test]
    fn artifact_manifest_rejects_parent_traversal() {
        let temporary = tempfile::tempdir().expect("temp");
        let manifest = temporary.path().join("manifest.json");
        fs::write(
            &manifest,
            br#"{"protocolMajor":1,"buildId":"b","artifacts":[{"target":"aarch64-apple-darwin","path":"../escape","length":1,"sha256":"00"}]}"#,
        )
        .expect("manifest");
        assert!(ArtifactCatalog::from_manifest(&manifest).is_err());
    }

    #[test]
    fn release_manifest_requires_all_supported_targets() {
        let temporary = tempfile::tempdir().expect("temp");
        let artifact = temporary.path().join("helper");
        fs::write(&artifact, b"helper").expect("artifact");
        let hash = hex_sha256(b"helper");
        let manifest = temporary.path().join("manifest.json");
        fs::write(
            &manifest,
            format!(
                r#"{{"protocolMajor":1,"buildId":"b","artifacts":[{{"target":"aarch64-apple-darwin","path":"helper","length":6,"sha256":"{hash}"}}]}}"#
            ),
        )
        .expect("manifest");
        let error = ArtifactCatalog::from_manifest(&manifest).expect_err("incomplete catalog");
        assert!(error.to_string().contains("missing required target"));
    }

    #[test]
    fn persistence_probe_surfaces_all_three_capability_outcomes() {
        assert_eq!(
            classify_persistence(true, false),
            PersistenceCapability::NativeDetach
        );
        assert_eq!(
            classify_persistence(false, true),
            PersistenceCapability::UserSupervisor
        );
        assert_eq!(
            classify_persistence(false, false),
            PersistenceCapability::NonPersistent
        );
    }

    #[test]
    fn helper_probe_without_directory_management_is_rejected_before_rpc() {
        let temporary = tempfile::tempdir().expect("temp");
        let path = temporary.path().join("helper");
        fs::write(&path, b"helper").expect("artifact");
        let artifact = PackagedArtifact {
            target: RemoteTarget::MacosAarch64,
            protocol_major: diri_proto::remote_pty::PROTOCOL_MAJOR,
            build_id: "legacy-build".into(),
            length: 6,
            sha256: hex_sha256(b"helper"),
            path,
        };
        let probe = HelperProbe {
            protocol: ProtocolVersion::CURRENT,
            build_id: artifact.build_id.clone(),
            artifact_sha256: artifact.sha256.clone(),
            target: artifact.target.artifact_name().into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            supported: true,
            holder_available: true,
            capabilities: PHASE_ONE_HELPER_CAPABILITIES
                .iter()
                .copied()
                .filter(|capability| {
                    *capability != diri_proto::remote_pty::RemoteCapability::DirectoryList
                })
                .collect(),
        };

        let error = verify_required_helper_probe(&artifact, &probe)
            .expect_err("a legacy Helper must fail before directories RPC");
        assert!(error.to_string().contains("directory-list"));
    }

    #[test]
    fn long_control_directories_use_a_short_stable_owner_path() {
        let requested = PathBuf::from("/very-long").join("segment".repeat(20));
        let first = normalized_control_dir(&requested);
        let second = normalized_control_dir(&requested);
        assert_eq!(first, second);
        assert!(first.starts_with("/tmp"));
        assert!(first.as_os_str().as_bytes().len() < MAX_CONTROL_DIRECTORY_BYTES);
        assert_ne!(first, normalized_control_dir(&requested.join("different")));
    }

    #[cfg(unix)]
    #[test]
    fn fake_ssh_bootstrap_uploads_activates_and_then_reuses_exact_build() {
        let temporary = tempfile::tempdir().expect("temp");
        let remote_home = temporary.path().join("remote-home");
        fs::create_dir(&remote_home).expect("remote home");
        // Use a fixed supported target instead of leaking the developer
        // machine's architecture into this fake-host bootstrap test.
        let target = RemoteTarget::MacosAarch64;
        let artifact_path = temporary.path().join("diri-remote-fixture");
        let artifact_script = format!(
            "#!/bin/sh\ncase \"$1\" in\nprobe) printf '%s\\n' '{{\"protocol\":{{\"major\":1,\"minor\":3}},\"buildId\":\"test-build\",\"artifactSha256\":\"'$TEST_ARTIFACT_SHA'\",\"target\":\"{}\",\"os\":\"test\",\"arch\":\"test\",\"supported\":true,\"holderAvailable\":true,\"capabilities\":[\"full-snapshot\",\"incremental-grid\",\"process-exit\",\"signal\",\"controller-lease\",\"scrollback\",\"session-management\",\"environment-capture\",\"directory-list\",\"executable-discovery\",\"persistence-probe\",\"atomic-activation\"]}}';;\nactivate) final=$(dirname \"$0\")/diri-remote; ln \"$0\" \"$final\" 2>/dev/null || true; rm -f \"$0\"; exec \"$final\" probe --format=json;;\n*) exit 64;;\nesac\n",
            target.artifact_name()
        );
        fs::write(&artifact_path, artifact_script).expect("artifact");
        fs::set_permissions(&artifact_path, fs::Permissions::from_mode(0o700))
            .expect("artifact mode");
        let artifact_sha = hex_sha256(&fs::read(&artifact_path).expect("artifact bytes"));
        let upload_log = temporary.path().join("uploads.log");

        let fake_ssh = temporary.path().join("ssh");
        let mut fake = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&fake_ssh)
            .expect("fake ssh");
        writeln!(
            fake,
            "#!/bin/sh\nexport HOME='{}'\nexport TEST_ARTIFACT_SHA='{}'\nfor last; do :; done\ncase \"$last\" in *'cat >'*) printf 'upload\\n' >> '{}';; esac\ncase \"$last\" in\n  *'__DIRI_PLATFORM_V1__'*) printf '__DIRI_PLATFORM_V1__\\0Darwin\\0aarch64\\0%s\\0' \"$HOME\";;\n  *) exec /bin/sh -c \"$last\";;\nesac",
            remote_home.display(),
            artifact_sha,
            upload_log.display()
        )
        .expect("fake script");
        drop(fake);

        let artifact = PackagedArtifact {
            target,
            protocol_major: 1,
            build_id: "test-build".into(),
            length: fs::metadata(&artifact_path).expect("metadata").len(),
            sha256: artifact_sha,
            path: artifact_path,
        };
        artifact.verify().expect("artifact verifies");
        let catalog = ArtifactCatalog {
            artifacts: HashMap::from([(target, artifact)]),
        };
        let manager = RemoteManager::new(
            ProcessExecutor::new(&fake_ssh),
            catalog,
            temporary.path().join("control"),
        )
        .expect("manager");
        let host = HostEntry {
            id: "fixture".into(),
            name: None,
            ssh: "fake-host".into(),
            default_cwd: None,
            node: None,
        };
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let installs = (0..4)
            .map(|_| {
                let manager = manager.clone();
                let host = host.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    manager.ensure_helper(&host)
                })
            })
            .collect::<Vec<_>>();
        let installed = installs
            .into_iter()
            .map(|thread| thread.join().expect("bootstrap thread").expect("bootstrap"))
            .collect::<Vec<_>>();
        let first = &installed[0];
        assert!(
            installed
                .iter()
                .all(|helper| helper.build_id == first.build_id)
        );
        assert_eq!(first.build_id, "test-build");
        let final_path = remote_home.join(".cache/diri/bin/protocol-1/test-build/diri-remote");
        assert!(final_path.is_file());
        let second = manager.ensure_helper(&host).expect("idempotent bootstrap");
        assert_eq!(second.build_id, first.build_id);
        let uploads_before_reinstall = fs::read_to_string(&upload_log)
            .expect("upload log")
            .lines()
            .count();
        let reinstalled = manager
            .reinstall_helper(&host)
            .expect("forced verified reinstall");
        assert_eq!(reinstalled.build_id, first.build_id);
        let uploads_after_reinstall = fs::read_to_string(&upload_log)
            .expect("upload log")
            .lines()
            .count();
        assert_eq!(
            uploads_after_reinstall,
            uploads_before_reinstall + 1,
            "reinstall must stage the packaged bytes even when the exact build exists"
        );
        let after_reinstall = manager.ensure_helper(&host).expect("version-gated reuse");
        assert_eq!(after_reinstall.build_id, first.build_id);
        assert_eq!(
            fs::read_to_string(&upload_log)
                .expect("upload log")
                .lines()
                .count(),
            uploads_after_reinstall,
            "a normal version check must reuse the exact verified build"
        );
        assert!(
            fs::read_dir(final_path.parent().expect("parent"))
                .expect("version dir")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp-"))
        );
    }

    fn hex_sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}
