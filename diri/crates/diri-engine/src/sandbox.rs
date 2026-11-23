//! Ghost Desktops behind `sandbox.*`: a bounded pool of isolated surfaces
//! Agents drive in parallel without touching the person's screen.
//!
//! The pool owns every sandbox. A slot is reserved before a backend launches
//! (so a burst of starts never overshoots capacity), filled when the launch
//! succeeds, and released on any failure. Each sandbox is leased to the
//! session that started it: other sessions get [`NOT_OWNER`], and the
//! person (a call without a session) may act on any of them, which takes it
//! over for a moment.
//!
//! Reclamation has three independent paths, so nothing outlives its reason:
//! - `stop`, an owner's session ending, or a long quiet spell drop the
//!   sandbox through [`SandboxInstance::shutdown`];
//! - a reaper thread runs only while at least one sandbox exists (no idle
//!   polling) and checks owners, idleness and backend death once a second;
//! - each backend ties its processes to the Engine (the ghost backend's
//!   browser exits when its pipe closes), so even a killed Engine leaks no
//!   process, and stale private state directories are swept at next start.
//!
//! `formal/SandboxPool.lean` models the slot and lease rules proved here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use diri_proto::sandbox::{
    self as proto, SandboxAction, SandboxBackendKind, SandboxBackendStatus, SandboxFrame,
    SandboxInfo, SandboxListResult, SandboxSnapshot, SandboxUsage,
};
use diri_proto::{DateMillis, SessionId};
use serde_json::Value;

pub mod ghost;
pub mod usage;

/// No action for this long → the sandbox is trimmed (caches and JS heaps
/// dropped) but keeps running, so its state survives a pause to think.
const SLEEP_AFTER: Duration = Duration::from_secs(30);
/// No action for this long → it is stopped and its slot freed.
const RECLAIM_AFTER: Duration = Duration::from_secs(10 * 60);
/// The reaper's beat while any sandbox exists.
const REAP_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxError {
    pub code: &'static str,
    pub message: String,
}

impl SandboxError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(proto::ACTION_FAILED, message)
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// What a backend needs to bring one sandbox up.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub id: String,
    pub width: u32,
    pub height: u32,
    /// Private, owner-only directory for this sandbox's state, already
    /// created. The backend must keep everything it writes inside it.
    pub state_dir: PathBuf,
}

/// One isolation mechanism. Implementations must fail closed: when the
/// mechanism is missing, `availability` says why and `launch` refuses.
pub trait SandboxBackend: Send + Sync {
    fn kind(&self) -> SandboxBackendKind;
    /// `Ok(detail)` names what runs it; `Err(reason)` says why it can't.
    fn availability(&self) -> Result<String, String>;
    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn SandboxInstance>, SandboxError>;
}

/// A running sandbox. Every method is called with the instance's own lock
/// held, never the pool's, so sandboxes act in parallel.
pub trait SandboxInstance: Send {
    fn act(&mut self, action: &SandboxAction) -> Result<Value, SandboxError>;
    /// A full-resolution PNG.
    fn screenshot_png(&mut self) -> Result<Vec<u8>, SandboxError>;
    /// A small JPEG for a model or a preview, at most `max_edge` wide/high.
    fn screenshot_jpeg(&mut self, max_edge: u32) -> Result<Vec<u8>, SandboxError>;
    /// Live preview frames, readable without this instance's lock.
    fn frames(&self) -> Option<Arc<dyn FrameSource>> {
        None
    }
    /// Current URL and title, for listings.
    fn location(&mut self) -> (String, String);
    /// The root of the sandbox's process tree, for measuring it.
    fn pid(&self) -> Option<u32> {
        None
    }
    /// Drop caches after a quiet spell. Optional.
    fn trim(&mut self) {}
    /// Called by the reaper; stop producing frames nobody asked for.
    fn tick(&mut self) {}
    fn alive(&mut self) -> bool;
    /// Reclaim every process and byte this sandbox owns. Must not fail.
    fn shutdown(&mut self);
}

/// One JPEG preview frame.
#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub jpeg: Arc<Vec<u8>>,
}

/// A sandbox's live preview. `want` keeps frames coming for a moment;
/// nobody asking means nothing is rendered for previews at all.
pub trait FrameSource: Send + Sync {
    fn latest(&self) -> Option<PreviewFrame>;
    fn want(&self);
}

/// Session liveness, from the Registry. Takes no pool lock.
pub type Liveness = dyn Fn(&str) -> bool + Send + Sync;

struct Slot {
    owner: Option<SessionId>,
    label: Option<String>,
    backend: SandboxBackendKind,
    width: u32,
    height: u32,
    started_at: SystemTime,
    spin_up: Duration,
    last_active: Instant,
    last_active_wall: SystemTime,
    human_until: Option<Instant>,
    sleeping: bool,
    state_dir: PathBuf,
    instance: Arc<Mutex<Box<dyn SandboxInstance>>>,
    frames: Option<Arc<dyn FrameSource>>,
    pid: Option<u32>,
    /// Last known location, for listings while an action holds the
    /// instance.
    url: String,
    title: String,
    snapshots: u32,
}

#[derive(Default)]
struct PoolState {
    slots: HashMap<String, Slot>,
    /// Starts whose launch is in flight; they count against capacity.
    reserved: usize,
    next_id: u64,
    reaper_running: bool,
    swept: bool,
}

struct Shared {
    capacity: usize,
    backends: Vec<Box<dyn SandboxBackend>>,
    state_root: PathBuf,
    artifact_root: PathBuf,
    state: Mutex<PoolState>,
    liveness: Mutex<Option<Arc<Liveness>>>,
    sleep_after: Duration,
    reclaim_after: Duration,
    reap_interval: Duration,
}

#[derive(Clone)]
pub struct SandboxPool {
    shared: Arc<Shared>,
}

/// Who is calling: a session, or the person.
#[derive(Clone, Copy, Debug)]
pub enum Caller<'a> {
    Session(&'a str),
    Person,
}

impl<'a> Caller<'a> {
    pub fn from_option(session: Option<&'a SessionId>) -> Self {
        session.map_or(Self::Person, |id| Self::Session(id.0.as_str()))
    }
}

pub struct PoolConfig {
    pub capacity: usize,
    pub state_root: PathBuf,
    pub artifact_root: PathBuf,
    pub sleep_after: Duration,
    pub reclaim_after: Duration,
    pub reap_interval: Duration,
}

impl PoolConfig {
    /// Production settings: four slots, private state under the user's
    /// temporary directory, snapshots next to the Engine's logs.
    pub fn for_engine(logs_dir: &Path) -> Self {
        Self {
            capacity: proto::DEFAULT_CAPACITY,
            state_root: default_state_root(),
            artifact_root: logs_dir.join("sandbox-artifacts"),
            sleep_after: SLEEP_AFTER,
            reclaim_after: RECLAIM_AFTER,
            reap_interval: REAP_INTERVAL,
        }
    }
}

/// `$TMPDIR/diri-sandboxes-<uid>`: per-user on macOS already, and the uid
/// keeps a shared Linux `/tmp` from mixing users.
pub fn default_state_root() -> PathBuf {
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    std::env::temp_dir().join(format!("diri-sandboxes-{uid}"))
}

impl SandboxPool {
    pub fn new(config: PoolConfig, backends: Vec<Box<dyn SandboxBackend>>) -> Self {
        Self {
            shared: Arc::new(Shared {
                capacity: config.capacity,
                backends,
                state_root: config.state_root,
                artifact_root: config.artifact_root,
                state: Mutex::new(PoolState::default()),
                liveness: Mutex::new(None),
                sleep_after: config.sleep_after,
                reclaim_after: config.reclaim_after,
                reap_interval: config.reap_interval,
            }),
        }
    }

    /// The Engine's pool: the ghost backend, and the micro-VM backend that
    /// reports why it can't run here.
    pub fn for_engine(logs_dir: &Path) -> Self {
        Self::new(
            PoolConfig::for_engine(logs_dir),
            vec![
                Box::new(ghost::GhostBackend::discover()),
                Box::new(MicroVmBackend),
            ],
        )
    }

    pub fn set_liveness(&self, liveness: Arc<Liveness>) {
        *self
            .shared
            .liveness
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(liveness);
    }

    fn state(&self) -> MutexGuard<'_, PoolState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    /// Live sandboxes plus starts in flight.
    pub fn occupied(&self) -> usize {
        let state = self.state();
        state.slots.len() + state.reserved
    }

    pub fn backends(&self) -> Vec<SandboxBackendStatus> {
        self.shared
            .backends
            .iter()
            .map(|backend| match backend.availability() {
                Ok(detail) => SandboxBackendStatus {
                    kind: backend.kind(),
                    available: true,
                    detail,
                },
                Err(detail) => SandboxBackendStatus {
                    kind: backend.kind(),
                    available: false,
                    detail,
                },
            })
            .collect()
    }

    pub fn start(
        &self,
        caller: Caller<'_>,
        params: &proto::SandboxStartParams,
    ) -> Result<SandboxInfo, SandboxError> {
        let backend = self
            .shared
            .backends
            .iter()
            .find(|backend| backend.kind() == params.backend)
            .ok_or_else(|| {
                SandboxError::new(
                    proto::BACKEND_UNAVAILABLE,
                    format!("no {} backend in this build", params.backend.as_str()),
                )
            })?;
        backend
            .availability()
            .map_err(|reason| SandboxError::new(proto::BACKEND_UNAVAILABLE, reason))?;
        if let Caller::Session(id) = caller
            && !self.is_live(id)
        {
            return Err(SandboxError::new(
                proto::NOT_OWNER,
                format!("session {id} is not live"),
            ));
        }
        // Reserve before launching: capacity holds even under a burst.
        let id = {
            let mut state = self.state();
            if state.slots.len() + state.reserved >= self.shared.capacity {
                return Err(SandboxError::new(
                    proto::POOL_FULL,
                    format!(
                        "all {} sandboxes are in use; stop one (sandbox.stop) or wait",
                        self.shared.capacity
                    ),
                ));
            }
            state.reserved += 1;
            state.next_id += 1;
            let sweep = !std::mem::replace(&mut state.swept, true);
            let id = format!("g{}", state.next_id);
            drop(state);
            if sweep {
                sweep_stale_state(&self.shared.state_root);
            }
            id
        };
        let reservation = Reservation { pool: self };
        let width = proto::clamp_dimension(params.width, proto::DEFAULT_WIDTH);
        let height = proto::clamp_dimension(params.height, proto::DEFAULT_HEIGHT);
        let state_dir = create_state_dir(&self.shared.state_root, &id)
            .map_err(|error| SandboxError::failed(format!("sandbox state directory: {error}")))?;
        let spec = LaunchSpec {
            id: id.clone(),
            width,
            height,
            state_dir: state_dir.clone(),
        };
        let launched = Instant::now();
        let mut instance = match backend.launch(&spec) {
            Ok(instance) => instance,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(error);
            }
        };
        if let Some(url) = params.url.as_deref().filter(|url| !url.is_empty())
            && let Err(error) = instance.act(&SandboxAction::Navigate { url: url.into() })
        {
            instance.shutdown();
            let _ = std::fs::remove_dir_all(&state_dir);
            return Err(error);
        }
        let spin_up = launched.elapsed();
        let now = Instant::now();
        let frames = instance.frames();
        let pid = instance.pid();
        let slot = Slot {
            owner: match caller {
                Caller::Session(id) => Some(SessionId(id.to_owned())),
                Caller::Person => None,
            },
            label: params
                .label
                .as_deref()
                .map(|label| label.chars().take(40).collect()),
            backend: params.backend,
            width,
            height,
            started_at: SystemTime::now(),
            spin_up,
            last_active: now,
            last_active_wall: SystemTime::now(),
            human_until: None,
            sleeping: false,
            state_dir,
            instance: Arc::new(Mutex::new(instance)),
            frames,
            pid,
            url: params.url.clone().unwrap_or_else(|| "about:blank".into()),
            title: String::new(),
            snapshots: 0,
        };
        let start_reaper = {
            let mut state = self.state();
            reservation.fill(&mut state);
            state.slots.insert(id.clone(), slot);
            !std::mem::replace(&mut state.reaper_running, true)
        };
        if start_reaper {
            self.spawn_reaper();
        }
        diri_telemetry::event!(
            "sandbox.start",
            backend = params.backend.as_str(),
            spin_up_ms = spin_up.as_millis() as u64
        );
        self.info(&id)
    }

    pub fn act(
        &self,
        caller: Caller<'_>,
        id: &str,
        action: &SandboxAction,
    ) -> Result<Value, SandboxError> {
        let instance = self.lease(caller, id)?;
        let result = lock_instance(&instance).act(action);
        self.mark_active(id);
        result
    }

    pub fn snapshot(
        &self,
        caller: Caller<'_>,
        id: &str,
        label: Option<&str>,
        inline: bool,
    ) -> Result<SandboxSnapshot, SandboxError> {
        use base64::Engine as _;
        use sha2::Digest as _;
        let instance = self.lease(caller, id)?;
        let (png, jpeg) = {
            let mut instance = lock_instance(&instance);
            let png = instance.screenshot_png()?;
            let jpeg = if inline {
                Some(instance.screenshot_jpeg(1024)?)
            } else {
                None
            };
            (png, jpeg)
        };
        let (width, height) = png_dimensions(&png).unwrap_or((0, 0));
        let index = {
            let mut state = self.state();
            let slot = state
                .slots
                .get_mut(id)
                .ok_or_else(|| SandboxError::new(proto::NOT_FOUND, format!("no sandbox {id}")))?;
            slot.snapshots += 1;
            slot.snapshots
        };
        let label = label
            .map(sanitize_label)
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "snapshot".into());
        let dir = self.shared.artifact_root.join(id);
        create_private_dir_all(&dir)
            .map_err(|error| SandboxError::failed(format!("snapshot directory: {error}")))?;
        let path = dir.join(format!("{index:03}-{label}.png"));
        write_private(&path, &png)
            .map_err(|error| SandboxError::failed(format!("snapshot write: {error}")))?;
        self.mark_active(id);
        Ok(SandboxSnapshot {
            path: path.display().to_string(),
            width,
            height,
            bytes: png.len() as u64,
            sha256: sha2::Sha256::digest(&png)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            jpeg: jpeg.map(|jpeg| base64::engine::general_purpose::STANDARD.encode(jpeg)),
        })
    }

    /// The live preview. Reading frames is not activity: a person watching
    /// does not keep an Agent's sandbox from being reclaimed.
    pub fn frame(&self, id: &str, after_seq: Option<u64>) -> Result<SandboxFrame, SandboxError> {
        use base64::Engine as _;
        let frames = self
            .state()
            .slots
            .get(id)
            .ok_or_else(|| SandboxError::new(proto::NOT_FOUND, format!("no sandbox {id}")))?
            .frames
            .clone();
        let Some(frames) = frames else {
            return Ok(SandboxFrame::default());
        };
        frames.want();
        Ok(match frames.latest() {
            None => SandboxFrame::default(),
            Some(frame) => SandboxFrame {
                seq: frame.seq,
                width: frame.width,
                height: frame.height,
                jpeg: (after_seq != Some(frame.seq)).then(|| {
                    base64::engine::general_purpose::STANDARD.encode(frame.jpeg.as_slice())
                }),
            },
        })
    }

    pub fn stop(&self, caller: Caller<'_>, id: &str) -> Result<(), SandboxError> {
        let slot = {
            let mut state = self.state();
            let slot = state
                .slots
                .get(id)
                .ok_or_else(|| SandboxError::new(proto::NOT_FOUND, format!("no sandbox {id}")))?;
            check_owner(caller, slot, id)?;
            state.slots.remove(id).expect("checked above")
        };
        reclaim(id, slot, "stop");
        Ok(())
    }

    /// Trims a sandbox now, as a quiet spell would: caches and JS heaps
    /// dropped, state kept. The next action wakes it.
    pub fn sleep(&self, id: &str) -> Result<(), SandboxError> {
        let instance = {
            let mut state = self.state();
            let slot = state
                .slots
                .get_mut(id)
                .ok_or_else(|| SandboxError::new(proto::NOT_FOUND, format!("no sandbox {id}")))?;
            slot.sleeping = true;
            Arc::clone(&slot.instance)
        };
        lock_instance(&instance).trim();
        Ok(())
    }

    /// Stops every sandbox a session holds; `session.remove` and the reaper
    /// call it. Returns how many were stopped.
    pub fn release_owner(&self, session: &str) -> usize {
        let slots: Vec<(String, Slot)> = {
            let mut state = self.state();
            let ids: Vec<String> = state
                .slots
                .iter()
                .filter(|(_, slot)| slot.owner.as_ref().is_some_and(|o| o.0 == session))
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| state.slots.remove(&id).map(|slot| (id, slot)))
                .collect()
        };
        let count = slots.len();
        for (id, slot) in slots {
            reclaim(&id, slot, "owner_ended");
        }
        count
    }

    /// Stops everything: Engine shutdown.
    pub fn shutdown(&self) {
        let slots: Vec<(String, Slot)> = self.state().slots.drain().collect();
        for (id, slot) in slots {
            reclaim(&id, slot, "shutdown");
        }
    }

    pub fn list(&self) -> SandboxListResult {
        let ids: Vec<String> = {
            let state = self.state();
            let mut ids: Vec<String> = state.slots.keys().cloned().collect();
            ids.sort_by_key(|id| {
                id.trim_start_matches('g')
                    .parse::<u64>()
                    .unwrap_or(u64::MAX)
            });
            ids
        };
        SandboxListResult {
            capacity: self.shared.capacity,
            sandboxes: ids.iter().filter_map(|id| self.info(id).ok()).collect(),
            backends: self.backends(),
        }
    }

    /// One sandbox, measured now. Never waits on an in-flight action: a
    /// busy sandbox reports its last known location.
    pub fn info(&self, id: &str) -> Result<SandboxInfo, SandboxError> {
        let (mut info, instance, pid, state_dir) = {
            let state = self.state();
            let slot = state
                .slots
                .get(id)
                .ok_or_else(|| SandboxError::new(proto::NOT_FOUND, format!("no sandbox {id}")))?;
            let now = Instant::now();
            (
                SandboxInfo {
                    id: id.to_owned(),
                    owner: slot.owner.clone(),
                    label: slot.label.clone(),
                    backend: slot.backend,
                    url: slot.url.clone(),
                    title: slot.title.clone(),
                    width: slot.width,
                    height: slot.height,
                    started_at: DateMillis::from(slot.started_at),
                    last_active_at: DateMillis::from(slot.last_active_wall),
                    spin_up_ms: slot.spin_up.as_millis() as u64,
                    usage: SandboxUsage::default(),
                    human_until: slot
                        .human_until
                        .filter(|until| *until > now)
                        .map(|until| DateMillis::from(SystemTime::now() + (until - now))),
                    sleeping: slot.sleeping,
                },
                Arc::clone(&slot.instance),
                slot.pid,
                slot.state_dir.clone(),
            )
        };
        let fresh = instance
            .try_lock()
            .ok()
            .map(|mut instance| instance.location());
        if let Some((url, title)) = fresh {
            if let Some(slot) = self.state().slots.get_mut(id) {
                slot.url.clone_from(&url);
                slot.title.clone_from(&title);
            }
            info.url = url;
            info.title = title;
        }
        info.usage = match pid {
            Some(pid) => usage::tree_usage(pid, &state_dir),
            None => SandboxUsage {
                disk_bytes: usage::disk_bytes(&state_dir),
                ..SandboxUsage::default()
            },
        };
        Ok(info)
    }

    /// The instance, if `caller` may act on it now. The person's action
    /// takes the sandbox over; an Agent's is refused while the person holds
    /// it.
    fn lease(
        &self,
        caller: Caller<'_>,
        id: &str,
    ) -> Result<Arc<Mutex<Box<dyn SandboxInstance>>>, SandboxError> {
        let mut state = self.state();
        let slot = state
            .slots
            .get_mut(id)
            .ok_or_else(|| SandboxError::new(proto::NOT_FOUND, format!("no sandbox {id}")))?;
        check_owner(caller, slot, id)?;
        let now = Instant::now();
        match caller {
            Caller::Person => {
                slot.human_until = Some(now + Duration::from_millis(proto::HUMAN_TAKEOVER_MS));
            }
            Caller::Session(_) => {
                if let Some(until) = slot.human_until.filter(|until| *until > now) {
                    return Err(SandboxError::new(
                        proto::HUMAN_CONTROL,
                        format!(
                            "the person is using sandbox {id}; retry in {} ms",
                            (until - now).as_millis()
                        ),
                    ));
                }
            }
        }
        Ok(Arc::clone(&slot.instance))
    }

    fn mark_active(&self, id: &str) {
        if let Some(slot) = self.state().slots.get_mut(id) {
            slot.last_active = Instant::now();
            slot.last_active_wall = SystemTime::now();
            slot.sleeping = false;
        }
    }

    fn is_live(&self, session: &str) -> bool {
        let liveness = self
            .shared
            .liveness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        liveness.is_none_or(|live| live(session))
    }

    fn spawn_reaper(&self) {
        let pool = self.clone();
        let spawned = std::thread::Builder::new()
            .name("diri-sandbox-reaper".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(pool.shared.reap_interval);
                    if !pool.reap_once() {
                        return;
                    }
                }
            });
        if spawned.is_err() {
            self.state().reaper_running = false;
        }
    }

    /// One reaper beat. Returns false (and marks the reaper stopped) once
    /// the pool is empty, so an idle Engine runs no timer at all.
    pub fn reap_once(&self) -> bool {
        let now = Instant::now();
        let mut expired = Vec::new();
        let mut sleepy = Vec::new();
        let mut instances = Vec::new();
        {
            let mut state = self.state();
            if state.slots.is_empty() && state.reserved == 0 {
                state.reaper_running = false;
                return false;
            }
            for (id, slot) in &state.slots {
                let idle = now.saturating_duration_since(slot.last_active);
                if idle >= self.shared.reclaim_after {
                    expired.push((id.clone(), "idle"));
                } else if idle >= self.shared.sleep_after && !slot.sleeping {
                    sleepy.push(id.clone());
                }
                instances.push((id.clone(), slot.owner.clone(), Arc::clone(&slot.instance)));
            }
        }
        for (id, owner, instance) in instances {
            if owner.as_ref().is_some_and(|owner| !self.is_live(&owner.0)) {
                expired.push((id, "owner_ended"));
                continue;
            }
            // A busy instance is alive by definition; don't wait on it.
            if let Ok(mut instance) = instance.try_lock() {
                if !instance.alive() {
                    expired.push((id, "exited"));
                } else {
                    instance.tick();
                    if sleepy.contains(&id) {
                        instance.trim();
                    }
                }
            }
        }
        if !sleepy.is_empty() {
            let mut state = self.state();
            for id in &sleepy {
                if let Some(slot) = state.slots.get_mut(id) {
                    slot.sleeping = true;
                }
            }
        }
        let slots: Vec<(String, Slot, &'static str)> = {
            let mut state = self.state();
            expired
                .into_iter()
                .filter_map(|(id, why)| state.slots.remove(&id).map(|slot| (id, slot, why)))
                .collect()
        };
        for (id, slot, why) in slots {
            reclaim(&id, slot, why);
        }
        true
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let slots: Vec<(String, Slot)> = self
            .state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .drain()
            .collect();
        for (id, slot) in slots {
            reclaim(&id, slot, "drop");
        }
    }
}

/// Releases a reserved slot unless the launch filled it.
struct Reservation<'a> {
    pool: &'a SandboxPool,
}

impl Reservation<'_> {
    fn fill(self, state: &mut PoolState) {
        state.reserved -= 1;
        std::mem::forget(self);
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.pool.state().reserved -= 1;
    }
}

fn check_owner(caller: Caller<'_>, slot: &Slot, id: &str) -> Result<(), SandboxError> {
    match (caller, &slot.owner) {
        (Caller::Person, _) => Ok(()),
        (Caller::Session(session), Some(owner)) if owner.0 == session => Ok(()),
        (Caller::Session(_), _) => Err(SandboxError::new(
            proto::NOT_OWNER,
            format!("sandbox {id} belongs to another session"),
        )),
    }
}

fn lock_instance(
    instance: &Arc<Mutex<Box<dyn SandboxInstance>>>,
) -> MutexGuard<'_, Box<dyn SandboxInstance>> {
    instance
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shuts the instance down and deletes its private state. Called with no
/// pool lock held; waits for an in-flight action on that one sandbox.
fn reclaim(id: &str, slot: Slot, why: &'static str) {
    lock_instance(&slot.instance).shutdown();
    let _ = std::fs::remove_dir_all(&slot.state_dir);
    let _ = id;
    diri_telemetry::event!("sandbox.stop", reason = why);
}

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect()
}

/// Width and height from a PNG's IHDR.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((width, height))
}

/// Owner-only directory, refusing a root that is a symlink or someone
/// else's.
fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// `<root>/<engine pid>-<id>`, owner-only. The root must be a real
/// directory we own: never follow a planted symlink into someone else's.
fn create_state_dir(root: &Path, id: &str) -> std::io::Result<PathBuf> {
    match std::fs::symlink_metadata(root) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(std::io::Error::other(format!(
                    "{} is not a directory",
                    root.display()
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                if meta.uid() != unsafe { libc::getuid() } {
                    return Err(std::io::Error::other(format!(
                        "{} belongs to another user",
                        root.display()
                    )));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_dir_all(root)?;
        }
        Err(error) => return Err(error),
    }
    let dir = root.join(format!("{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Removes `<pid>-<id>` directories left by Engines that are gone (a
/// killed Engine's browsers already exited with their pipes; their files
/// are what's left). Only names this module creates, only real directories.
pub fn sweep_stale_state(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((pid, rest)) = name.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        if !rest.starts_with('g') || pid == std::process::id() as i32 || process_alive(pid) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn process_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        if pid <= 0 {
            return false;
        }
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Linux micro-VMs through Virtualization.framework need a signed helper
/// with the `com.apple.security.virtualization` entitlement plus a kernel
/// and root filesystem image. Diri ships none of those, and downloading them
/// on first use would break the zero-setup rule, so this backend reports why
/// and refuses; it is never silently replaced by another mechanism.
pub struct MicroVmBackend;

impl SandboxBackend for MicroVmBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::MicroVm
    }

    fn availability(&self) -> Result<String, String> {
        Err(
            "micro-VM sandboxes need a signed Virtualization.framework helper and a Linux \
             kernel + root filesystem image, which this build does not ship; use the ghost \
             backend"
                .into(),
        )
    }

    fn launch(&self, _spec: &LaunchSpec) -> Result<Box<dyn SandboxInstance>, SandboxError> {
        Err(SandboxError::new(
            proto::BACKEND_UNAVAILABLE,
            self.availability().unwrap_err(),
        ))
    }
}

#[cfg(test)]
mod tests;
