//! The app-side driver for [`diri_updater`].
//!
//! Owns a tokio task that holds the [`Updater`], runs the blocking steps on
//! the blocking pool, and publishes a [`UpdateState`] the UI renders from. The
//! UI never calls the updater directly — it sends [`UpdateCommand`]s, so a
//! click cannot block a frame and two clicks cannot start two downloads.
//!
//! Automatic updates check, download, and verify in the background. Installing
//! waits for the user to quit or request a restart, so an update never steals
//! focus or interrupts a live session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diri_updater::{
    Release, Result as UpdateResult, StagedUpdate, UpdateError, Updater, UpdaterConfig,
};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

/// How long after launch the first background check runs. Long enough to stay
/// out of the way of startup work.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Small seam around the blocking updater so the service policy can be tested
/// without a network request or a signed app bundle.
trait UpdateBackend: Send + Sync {
    fn clean_cache(&self);
    fn check(&self, skipped: Option<&str>) -> UpdateResult<Option<Release>>;
    fn download_and_stage(
        &self,
        release: &Release,
        on_progress: &mut dyn FnMut(f32),
    ) -> UpdateResult<StagedUpdate>;
    fn install(&self, staged: &StagedUpdate, relaunch: bool) -> UpdateResult<()>;
}

impl UpdateBackend for Updater {
    fn clean_cache(&self) {
        Updater::clean_cache(self);
    }

    fn check(&self, skipped: Option<&str>) -> UpdateResult<Option<Release>> {
        Updater::check(self, skipped)
    }

    fn download_and_stage(
        &self,
        release: &Release,
        on_progress: &mut dyn FnMut(f32),
    ) -> UpdateResult<StagedUpdate> {
        let archive = Updater::download(self, release, on_progress)?;
        Updater::stage(self, release, &archive)
    }

    fn install(&self, staged: &StagedUpdate, relaunch: bool) -> UpdateResult<()> {
        Updater::install(self, staged, relaunch)
    }
}

#[derive(Clone)]
struct ReadyInstall {
    updater: Arc<dyn UpdateBackend>,
    staged: StagedUpdate,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum UpdatePhase {
    /// This build cannot update itself (unsigned, or not in a bundle). Carries
    /// the reason, shown in Settings so a dev build is not mistaken for a bug.
    Unsupported(String),
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading {
        release: Release,
        progress: f32,
    },
    /// Verified and staged; the swap happens on the next quit.
    Ready(Release),
    /// The helper is running and the app is about to be told to quit.
    Installing,
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpdateState {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub last_checked_unix: Option<u64>,
    /// True when this work should surface transient progress or failure. A
    /// background check stays quiet until it finds a release to download.
    pub user_initiated: bool,
}

impl UpdateState {
    /// The version the sidebar pill advertises, if there is one to advertise.
    pub fn pending_version(&self) -> Option<&str> {
        match &self.phase {
            UpdatePhase::Available(release)
            | UpdatePhase::Downloading { release, .. }
            | UpdatePhase::Ready(release) => Some(release.version.as_str()),
            _ => None,
        }
    }

    /// Whether the footer should show anything at all. A background check that
    /// found nothing stays silent.
    pub fn is_noteworthy(&self) -> bool {
        match &self.phase {
            UpdatePhase::Available(_)
            | UpdatePhase::Downloading { .. }
            | UpdatePhase::Ready(_)
            | UpdatePhase::Installing => true,
            UpdatePhase::Checking | UpdatePhase::UpToDate | UpdatePhase::Failed(_) => {
                self.user_initiated
            }
            UpdatePhase::Idle | UpdatePhase::Unsupported(_) => false,
        }
    }

    /// One line for the account footer and the Settings row.
    pub fn summary(&self) -> String {
        match &self.phase {
            UpdatePhase::Unsupported(_) => "Updates off for this build".to_owned(),
            UpdatePhase::Idle => format!("diri {}", self.current_version),
            UpdatePhase::Checking => "Checking for updates…".to_owned(),
            UpdatePhase::UpToDate => format!("diri {} is up to date", self.current_version),
            UpdatePhase::Available(release) => format!("Update to {}", release.version),
            UpdatePhase::Downloading { progress, .. } => {
                format!("Downloading… {}%", (progress * 100.0).round() as u32)
            }
            UpdatePhase::Ready(release) => format!("Restart to update to {}", release.version),
            UpdatePhase::Installing => "Restarting…".to_owned(),
            UpdatePhase::Failed(reason) => reason.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum UpdateCommand {
    Check {
        user_initiated: bool,
    },
    Download,
    Install,
    /// Never offer the pending version again.
    Skip,
    /// Clear a finished check's transient state (up to date / failed).
    Dismiss,
    SetAutomatic(bool),
}

/// UI-side handle: a state stream plus a command sink.
#[derive(Clone)]
pub struct UpdateHandle {
    state: watch::Receiver<UpdateState>,
    commands: mpsc::UnboundedSender<UpdateCommand>,
    ready_install: Arc<Mutex<Option<ReadyInstall>>>,
    automatic: Arc<AtomicBool>,
    // Preview mode keeps the watch open without spawning the updater service.
    _inert_state: Option<watch::Sender<UpdateState>>,
}

impl UpdateHandle {
    pub fn state(&self) -> UpdateState {
        self.state.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<UpdateState> {
        self.state.clone()
    }

    pub fn send(&self, command: UpdateCommand) {
        if let UpdateCommand::SetAutomatic(enabled) = &command {
            self.automatic.store(*enabled, Ordering::SeqCst);
        }
        // A closed channel means the service task is gone; the UI has nothing
        // useful to do about that, and the state stream stops updating anyway.
        let _ = self.commands.send(command);
    }

    pub fn check(&self, user_initiated: bool) {
        self.send(UpdateCommand::Check { user_initiated });
    }

    /// Launches the non-reopening swap helper when an automatic update is
    /// staged. This path is synchronous and bounded so quitting never waits
    /// behind an in-progress network download on the service task.
    pub fn install_on_quit(&self) {
        if !self.automatic.load(Ordering::SeqCst) {
            return;
        }
        let ready = self.ready_install.lock().expect("ready update").take();
        let Some(ready) = ready else {
            return;
        };
        if let Err(error) = ready.updater.install(&ready.staged, false) {
            eprintln!("diri updater: {error}");
            *self.ready_install.lock().expect("ready update") = Some(ready);
        }
    }
}

/// Starts the update service on `runtime` and returns the UI handle.
///
/// Never fails: a build that cannot update itself still gets a handle, parked
/// in [`UpdatePhase::Unsupported`], so no caller needs an `Option`.
pub fn spawn(runtime: &Arc<Runtime>, automatic: bool, skipped: Option<String>) -> UpdateHandle {
    let initial = UpdateState {
        current_version: CURRENT_VERSION.to_owned(),
        ..UpdateState::default()
    };
    let (state_tx, state_rx) = watch::channel(initial.clone());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let ready_install = Arc::new(Mutex::new(None));
    let automatic_flag = Arc::new(AtomicBool::new(automatic));

    let updater: Option<Arc<dyn UpdateBackend>> =
        match UpdaterConfig::for_running_app(CURRENT_VERSION) {
            Ok(config) => Some(Arc::new(Updater::new(config))),
            Err(error) => {
                state_tx.send_replace(UpdateState {
                    phase: UpdatePhase::Unsupported(error.to_string()),
                    ..initial
                });
                None
            }
        };

    let _guard = runtime.enter();
    tokio::spawn(service(
        updater,
        automatic,
        skipped,
        state_tx,
        command_rx,
        Arc::clone(&ready_install),
    ));
    UpdateHandle {
        state: state_rx,
        commands: command_tx,
        ready_install,
        automatic: automatic_flag,
        _inert_state: None,
    }
}

/// A stable, task-free update handle for deterministic UI previews and
/// performance probes. It neither touches the network nor creates a timer.
pub fn inert() -> UpdateHandle {
    let state = UpdateState {
        current_version: CURRENT_VERSION.to_owned(),
        ..UpdateState::default()
    };
    let (state_tx, state_rx) = watch::channel(state);
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    UpdateHandle {
        state: state_rx,
        commands: command_tx,
        ready_install: Arc::new(Mutex::new(None)),
        automatic: Arc::new(AtomicBool::new(false)),
        _inert_state: Some(state_tx),
    }
}

struct Service {
    updater: Arc<dyn UpdateBackend>,
    state: watch::Sender<UpdateState>,
    automatic: bool,
    /// Set once a check finds something, cleared when it is superseded.
    pending: Option<Release>,
    staged: Option<StagedUpdate>,
    skipped: Option<String>,
    busy: bool,
    ready_install: Arc<Mutex<Option<ReadyInstall>>>,
}

async fn service(
    updater: Option<Arc<dyn UpdateBackend>>,
    automatic: bool,
    skipped: Option<String>,
    state: watch::Sender<UpdateState>,
    mut commands: mpsc::UnboundedReceiver<UpdateCommand>,
    ready_install: Arc<Mutex<Option<ReadyInstall>>>,
) {
    let Some(updater) = updater else {
        // Nothing to drive, but keep draining commands so a click in Settings
        // on an unsupported build is a no-op instead of a channel error.
        while commands.recv().await.is_some() {}
        return;
    };
    {
        let updater = Arc::clone(&updater);
        let _ = tokio::task::spawn_blocking(move || updater.clean_cache()).await;
    }

    let mut service = Service {
        updater,
        state,
        automatic,
        pending: None,
        staged: None,
        // Persisted in Prefs, so a skip outlives the session that made it.
        skipped: skipped.filter(|version| !version.is_empty()),
        busy: false,
        ready_install,
    };

    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + FIRST_CHECK_DELAY,
        CHECK_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return };
                service.handle(command).await;
            }
            _ = ticker.tick() => {
                if service.automatic {
                    service.check(false).await;
                }
            }
        }
    }
}

impl Service {
    async fn handle(&mut self, command: UpdateCommand) {
        match command {
            UpdateCommand::Check { user_initiated } => self.check(user_initiated).await,
            UpdateCommand::Download => self.download(true).await,
            UpdateCommand::Install => self.install(true),
            UpdateCommand::Skip => {
                if let Some(release) = self.pending.take() {
                    self.skipped = Some(release.version);
                }
                self.staged = None;
                self.ready_install.lock().expect("ready update").take();
                self.publish(UpdatePhase::Idle, false);
            }
            UpdateCommand::Dismiss => {
                let phase = match self.state.borrow().phase.clone() {
                    // A staged or offered update survives a dismiss; only the
                    // transient results clear.
                    phase @ (UpdatePhase::Available(_)
                    | UpdatePhase::Downloading { .. }
                    | UpdatePhase::Ready(_)) => phase,
                    _ => UpdatePhase::Idle,
                };
                self.publish(phase, false);
            }
            UpdateCommand::SetAutomatic(enabled) => {
                self.automatic = enabled;
                if enabled && self.pending.is_some() && self.staged.is_none() {
                    self.download(true).await;
                }
            }
        }
    }

    async fn check(&mut self, user_initiated: bool) {
        // A staged update is the end of the line until the app restarts;
        // re-checking would only offer what is already on disk. Re-publish it
        // so a manual check still visibly answers rather than doing nothing.
        if let Some(staged) = &self.staged {
            self.publish(UpdatePhase::Ready(staged.release.clone()), user_initiated);
            return;
        }
        if self.busy {
            return;
        }
        self.busy = true;
        self.publish(UpdatePhase::Checking, user_initiated);

        let updater = Arc::clone(&self.updater);
        let skipped = self.skipped.clone();
        let found = tokio::task::spawn_blocking(move || updater.check(skipped.as_deref())).await;
        self.busy = false;
        self.touch_last_checked();

        match found {
            Ok(Ok(Some(release))) => {
                self.pending = Some(release.clone());
                self.publish(UpdatePhase::Available(release), user_initiated);
                if self.automatic {
                    // Once a release is found, progress and failures are worth
                    // surfacing even though the check itself was background.
                    self.download(true).await;
                }
            }
            Ok(Ok(None)) => {
                self.pending = None;
                self.publish(UpdatePhase::UpToDate, user_initiated);
            }
            Ok(Err(error)) => self.fail(&error, user_initiated),
            Err(_) => self.publish(
                UpdatePhase::Failed("The update check stopped unexpectedly".to_owned()),
                user_initiated,
            ),
        }
    }

    async fn download(&mut self, user_initiated: bool) {
        let Some(release) = self.pending.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        self.publish(
            UpdatePhase::Downloading {
                release: release.clone(),
                progress: 0.0,
            },
            user_initiated,
        );

        // Progress arrives on the blocking thread; funnel it through a channel
        // so only this task ever writes the state.
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let updater = Arc::clone(&self.updater);
        let downloading = release.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut on_progress = |fraction| {
                let _ = progress_tx.send(fraction);
            };
            updater.download_and_stage(&downloading, &mut on_progress)
        });
        tokio::pin!(worker);

        let staged = loop {
            tokio::select! {
                Some(fraction) = progress_rx.recv() => {
                    self.publish(
                        UpdatePhase::Downloading { release: release.clone(), progress: fraction },
                        user_initiated,
                    );
                }
                result = &mut worker => break result,
            }
        };
        self.busy = false;

        match staged {
            Ok(Ok(staged)) => {
                *self.ready_install.lock().expect("ready update") = Some(ReadyInstall {
                    updater: Arc::clone(&self.updater),
                    staged: staged.clone(),
                });
                self.staged = Some(staged);
                self.publish(UpdatePhase::Ready(release), user_initiated);
            }
            Ok(Err(error)) => self.fail(&error, user_initiated),
            Err(_) => self.publish(
                UpdatePhase::Failed("The download stopped unexpectedly".to_owned()),
                user_initiated,
            ),
        }
    }

    fn install(&mut self, relaunch: bool) {
        let ready = self.ready_install.lock().expect("ready update").take();
        let Some(ready) = ready else {
            return;
        };
        match ready.updater.install(&ready.staged, relaunch) {
            // RootView watches for Installing and quits; the helper is already
            // waiting for this process to go away.
            Ok(()) => {
                // Taking the staged update makes the app-quit hook idempotent
                // after an explicit "Restart to update" click.
                self.staged = None;
                self.publish(UpdatePhase::Installing, true);
            }
            Err(error) => {
                *self.ready_install.lock().expect("ready update") = Some(ready);
                self.fail(&error, true);
            }
        }
    }

    fn fail(&mut self, error: &UpdateError, user_initiated: bool) {
        eprintln!("diri updater: {error}");
        self.publish(UpdatePhase::Failed(error.user_facing()), user_initiated);
    }

    fn publish(&self, phase: UpdatePhase, user_initiated: bool) {
        let previous = self.state.borrow().clone();
        self.state.send_replace(UpdateState {
            phase,
            user_initiated,
            ..previous
        });
    }

    fn touch_last_checked(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .ok();
        let previous = self.state.borrow().clone();
        self.state.send_replace(UpdateState {
            last_checked_unix: now,
            ..previous
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    struct FakeUpdater {
        offered: Release,
        downloads: AtomicUsize,
        installs: Mutex<Vec<bool>>,
    }

    impl UpdateBackend for FakeUpdater {
        fn clean_cache(&self) {}

        fn check(&self, _skipped: Option<&str>) -> UpdateResult<Option<Release>> {
            Ok(Some(self.offered.clone()))
        }

        fn download_and_stage(
            &self,
            release: &Release,
            on_progress: &mut dyn FnMut(f32),
        ) -> UpdateResult<StagedUpdate> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            on_progress(1.0);
            Ok(StagedUpdate {
                release: release.clone(),
                app: PathBuf::from("/tmp/fake-staged/diri.app"),
                directory: PathBuf::from("/tmp/fake-staged"),
            })
        }

        fn install(&self, _staged: &StagedUpdate, relaunch: bool) -> UpdateResult<()> {
            self.installs.lock().expect("installs").push(relaunch);
            Ok(())
        }
    }

    fn release(version: &str) -> Release {
        Release {
            version: version.to_owned(),
            ..Release::default()
        }
    }

    fn state(phase: UpdatePhase, user_initiated: bool) -> UpdateState {
        UpdateState {
            phase,
            current_version: "0.4.2".to_owned(),
            last_checked_unix: None,
            user_initiated,
        }
    }

    #[test]
    fn a_quiet_background_check_shows_nothing() {
        assert!(!state(UpdatePhase::Checking, false).is_noteworthy());
        assert!(!state(UpdatePhase::UpToDate, false).is_noteworthy());
        assert!(!state(UpdatePhase::Failed("nope".into()), false).is_noteworthy());
        assert!(!state(UpdatePhase::Idle, false).is_noteworthy());
    }

    #[test]
    fn a_manual_check_reports_its_outcome_either_way() {
        assert!(state(UpdatePhase::UpToDate, true).is_noteworthy());
        assert!(state(UpdatePhase::Failed("nope".into()), true).is_noteworthy());
    }

    #[test]
    fn a_found_update_surfaces_even_from_a_background_check() {
        let found = state(UpdatePhase::Available(release("0.5.0")), false);
        assert!(found.is_noteworthy());
        assert_eq!(found.pending_version(), Some("0.5.0"));
        assert_eq!(found.summary(), "Update to 0.5.0");
    }

    #[test]
    fn a_dev_build_stays_silent_in_the_sidebar() {
        let unsupported = state(UpdatePhase::Unsupported("unsigned".into()), true);
        assert!(!unsupported.is_noteworthy());
        assert_eq!(unsupported.summary(), "Updates off for this build");
    }

    #[test]
    fn download_progress_reads_as_a_whole_percentage() {
        let downloading = state(
            UpdatePhase::Downloading {
                release: release("0.5.0"),
                progress: 0.436,
            },
            true,
        );
        assert_eq!(downloading.summary(), "Downloading… 44%");
        assert_eq!(downloading.pending_version(), Some("0.5.0"));
    }

    #[test]
    fn a_staged_update_asks_for_a_restart() {
        assert_eq!(
            state(UpdatePhase::Ready(release("0.5.0")), true).summary(),
            "Restart to update to 0.5.0"
        );
    }

    #[test]
    fn an_automatic_check_downloads_and_stages_the_release() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let updater = Arc::new(FakeUpdater {
                offered: release("0.5.0"),
                downloads: AtomicUsize::new(0),
                installs: Mutex::new(Vec::new()),
            });
            let backend: Arc<dyn UpdateBackend> = updater.clone();
            let (state_tx, mut state_rx) = watch::channel(UpdateState::default());
            let (command_tx, command_rx) = mpsc::unbounded_channel();
            let ready_install = Arc::new(Mutex::new(None));
            let task = tokio::spawn(service(
                Some(backend),
                true,
                None,
                state_tx,
                command_rx,
                ready_install,
            ));

            command_tx
                .send(UpdateCommand::Check {
                    user_initiated: false,
                })
                .expect("send background check");
            let ready = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if matches!(state_rx.borrow().phase, UpdatePhase::Ready(_)) {
                        return;
                    }
                    state_rx.changed().await.expect("service remains alive");
                }
            })
            .await;

            drop(command_tx);
            task.abort();
            assert!(
                ready.is_ok(),
                "automatic updates must be staged in the background"
            );
            assert_eq!(updater.downloads.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn disabling_automatic_updates_keeps_download_manual() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let updater = Arc::new(FakeUpdater {
                offered: release("0.5.0"),
                downloads: AtomicUsize::new(0),
                installs: Mutex::new(Vec::new()),
            });
            let backend: Arc<dyn UpdateBackend> = updater.clone();
            let (state_tx, mut state_rx) = watch::channel(UpdateState::default());
            let (command_tx, command_rx) = mpsc::unbounded_channel();
            let task = tokio::spawn(service(
                Some(backend),
                false,
                None,
                state_tx,
                command_rx,
                Arc::new(Mutex::new(None)),
            ));

            command_tx
                .send(UpdateCommand::Check {
                    user_initiated: true,
                })
                .expect("send manual check");
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if matches!(state_rx.borrow().phase, UpdatePhase::Available(_)) {
                        return;
                    }
                    state_rx.changed().await.expect("service remains alive");
                }
            })
            .await
            .expect("manual release offer");

            drop(command_tx);
            task.abort();
            assert_eq!(updater.downloads.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn disabling_automatic_updates_does_not_install_a_staged_release_on_quit() {
        let updater = Arc::new(FakeUpdater {
            offered: release("0.5.0"),
            downloads: AtomicUsize::new(0),
            installs: Mutex::new(Vec::new()),
        });
        let backend: Arc<dyn UpdateBackend> = updater.clone();
        let ready_install = Arc::new(Mutex::new(Some(ReadyInstall {
            updater: backend,
            staged: StagedUpdate {
                release: release("0.5.0"),
                app: PathBuf::from("/tmp/fake-staged/diri.app"),
                directory: PathBuf::from("/tmp/fake-staged"),
            },
        })));
        let (_state_tx, state) = watch::channel(UpdateState::default());
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let handle = UpdateHandle {
            state,
            commands,
            ready_install: Arc::clone(&ready_install),
            automatic: Arc::new(AtomicBool::new(false)),
            _inert_state: None,
        };

        handle.install_on_quit();
        assert!(updater.installs.lock().expect("installs").is_empty());
        assert!(
            ready_install.lock().expect("ready update").is_some(),
            "the verified release stays available for a manual restart"
        );
    }

    #[test]
    fn a_staged_automatic_update_is_installed_without_relaunch_on_normal_quit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let updater = Arc::new(FakeUpdater {
                offered: release("0.5.0"),
                downloads: AtomicUsize::new(0),
                installs: Mutex::new(Vec::new()),
            });
            let backend: Arc<dyn UpdateBackend> = updater.clone();
            let (state_tx, state_rx) = watch::channel(UpdateState::default());
            let (command_tx, command_rx) = mpsc::unbounded_channel();
            let ready_install = Arc::new(Mutex::new(None));
            let handle = UpdateHandle {
                state: state_rx,
                commands: command_tx,
                ready_install: Arc::clone(&ready_install),
                automatic: Arc::new(AtomicBool::new(true)),
                _inert_state: None,
            };
            let task = tokio::spawn(service(
                Some(backend),
                true,
                None,
                state_tx,
                command_rx,
                ready_install,
            ));

            handle.check(false);
            tokio::time::timeout(Duration::from_secs(1), async {
                let mut states = handle.subscribe();
                loop {
                    if matches!(states.borrow().phase, UpdatePhase::Ready(_)) {
                        return;
                    }
                    states.changed().await.expect("service remains alive");
                }
            })
            .await
            .expect("automatic update reaches ready");

            handle.install_on_quit();
            task.abort();
            assert_eq!(
                *updater.installs.lock().expect("installs"),
                vec![false],
                "a normal quit must swap the staged app without reopening it"
            );
        });
    }

    #[test]
    fn an_idle_build_just_names_its_version() {
        assert_eq!(state(UpdatePhase::Idle, false).summary(), "diri 0.4.2");
        assert_eq!(
            state(UpdatePhase::UpToDate, true).summary(),
            "diri 0.4.2 is up to date"
        );
    }

    #[test]
    fn inert_preview_handle_stays_idle_without_a_service_task() {
        let handle = inert();
        assert_eq!(handle.state().phase, UpdatePhase::Idle);
        assert!(!handle.subscribe().has_changed().expect("watch stays open"));
        handle.check(true);
        assert_eq!(handle.state().phase, UpdatePhase::Idle);
    }
}
