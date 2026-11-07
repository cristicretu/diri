//! Following the checkout an Agent actually works in.
//!
//! A Session's `cwd` is where it was launched. Agents leave it: Claude's
//! worktree isolation, `git worktree add ../fix && cd ../fix`, or edits to
//! another checkout by absolute path. The right panel used to stay on the
//! launch directory, so the Agent's changes were invisible there. New
//! Sessions (⌘T) still start in the launch directory: following is a view of
//! where the Agent works, not a move of the project.
//!
//! The "effective workspace" is resolved from evidence the app already has,
//! mapped into the launch repository's own `git worktree list`:
//!
//! * the launch directory, or the Diri-managed worktree it was created in;
//! * the directory the Agent's hooks last moved to (`agent_workspace.cwd`);
//! * a live process directory: a shell's `terminal_cwd`, or an Agent
//!   process's working directory read once per panel reveal;
//! * files the Agent recently edited (`agent_workspace.edits`).
//!
//! Every piece of evidence that falls in a worktree of the same repository
//! makes that worktree a candidate; the most recently active candidate wins,
//! ties going to the stronger kind of evidence. Nothing here polls: a
//! resolution runs when its inputs change (a hook moved the Agent, a new file
//! was edited) or when the panel is revealed, off the main thread.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diri_proto::{AgentKind, SessionId, SessionRecord};
use diri_ui::{FloatingSurface, Radius, SemanticColors, Typo};
use gpui::{AnyElement, Context, MouseButton, SharedString, Task, deferred, div, prelude::*, px};

use crate::icons::sf_symbol;
use crate::store::StoreRuntime;

/// One checkout listed by `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Worktree {
    pub path: PathBuf,
    /// Short branch name; `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
}

/// Parses `git worktree list --porcelain`. Records are blank-line separated;
/// unknown attributes (`locked`, `prunable`, future ones) are ignored.
pub(crate) fn parse_worktree_list(porcelain: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.extend(current.take());
            current = Some(Worktree {
                path: PathBuf::from(path),
                branch: None,
                bare: false,
            });
            continue;
        }
        let Some(worktree) = current.as_mut() else {
            continue;
        };
        if let Some(reference) = line.strip_prefix("branch ") {
            worktree.branch = Some(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_owned(),
            );
        } else if line == "bare" {
            worktree.bare = true;
        } else if line.is_empty() {
            worktrees.extend(current.take());
        }
    }
    worktrees.extend(current);
    worktrees
}

/// The checkout containing `path`: the deepest listed worktree that is a
/// component-wise prefix, so `.worktrees/fix` inside the main checkout wins
/// over the main checkout for its own files.
pub(crate) fn worktree_for<'a>(worktrees: &'a [Worktree], path: &Path) -> Option<&'a Worktree> {
    worktrees
        .iter()
        .filter(|worktree| !worktree.bare && path.starts_with(&worktree.path))
        .max_by_key(|worktree| worktree.path.components().count())
}

/// The kind of evidence behind a candidate. Order is tie priority: on equal
/// times a reported move outranks a sampled directory, which outranks an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Source {
    Launch,
    Edit,
    Process,
    Hook,
}

impl Source {
    fn is_directory(self) -> bool {
        matches!(self, Self::Process | Self::Hook)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Signal {
    pub path: PathBuf,
    /// Milliseconds since the epoch.
    pub at: u64,
    pub source: Source,
}

/// Everything a resolution depends on. Equal inputs resolve equally, so a
/// changed value is the only trigger for new work.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FollowInputs {
    pub launch_cwd: PathBuf,
    pub launch_at: u64,
    /// Launched into a Diri-managed worktree (`SessionRecord.worktree_path`).
    pub managed: bool,
    /// A shell, whose sampled directory is authoritative on its own.
    pub shell: bool,
    pub signals: Vec<Signal>,
}

impl FollowInputs {
    /// Local, process-backed Sessions only: a remote path names another
    /// machine, and a note has no checkout.
    pub(crate) fn of(session: &SessionRecord, process: Option<&Signal>) -> Option<Self> {
        if session.host.is_some() || session.is_note() || session.cwd.is_empty() {
            return None;
        }
        let mut signals = Vec::new();
        if let Some(workspace) = &session.agent_workspace {
            if let Some(cwd) = &workspace.cwd {
                signals.push(Signal {
                    path: PathBuf::from(&cwd.path),
                    at: millis(cwd.at.0),
                    source: Source::Hook,
                });
            }
            signals.extend(workspace.edits.iter().map(|edit| Signal {
                path: PathBuf::from(&edit.path),
                at: millis(edit.at.0),
                source: Source::Edit,
            }));
        }
        signals.extend(process.cloned());
        Some(Self {
            launch_cwd: PathBuf::from(&session.cwd),
            launch_at: millis(session.created_at.0),
            managed: session.worktree_path.is_some(),
            shell: session.kind == AgentKind::SHELL,
            signals,
        })
    }
}

/// Folds a sampled process directory into a [`Signal`] that keeps the time
/// the directory was first seen, like the Engine's hook cwd. An Agent process
/// that never left its launch directory is no evidence at all: Claude and
/// Codex keep their own cwd internally, and counting it would let an app
/// restart outvote a hook-reported move.
pub(crate) fn observe_process(
    previous: Option<&Signal>,
    sampled: Option<PathBuf>,
    launch_cwd: &Path,
    shell: bool,
    now: u64,
) -> Option<Signal> {
    let path = sampled?;
    if !shell && path == launch_cwd {
        return None;
    }
    Some(match previous {
        Some(previous) if previous.path == path => previous.clone(),
        _ => Signal {
            path,
            at: now,
            source: Source::Process,
        },
    })
}

/// One checkout the Agent has worked in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// Where the panel looks and a new tab starts: the launch directory for
    /// the launch checkout (unchanged behavior), else the directory the Agent
    /// moved to, else the launch subdirectory relocated into this checkout.
    pub directory: PathBuf,
    pub at: u64,
    pub source: Source,
    /// The checkout containing the launch directory.
    pub launch: bool,
    pub managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolution {
    /// The launch directory this was resolved for; a resolution for an older
    /// `cwd` (a reparented Session) is never applied.
    pub launch_cwd: PathBuf,
    /// Most recently active first. Empty outside a Git repository.
    pub candidates: Vec<Candidate>,
}

impl Resolution {
    pub(crate) fn launch_only(launch_cwd: PathBuf) -> Self {
        Self {
            launch_cwd,
            candidates: Vec::new(),
        }
    }

    /// The candidate to show: a pinned root when it is still a candidate,
    /// else the most recently active one.
    pub(crate) fn chosen(&self, pin: Option<&Path>) -> Option<&Candidate> {
        pin.and_then(|pin| self.candidates.iter().find(|c| c.root == pin))
            .or_else(|| self.candidates.first())
    }
}

/// Resolves `inputs` against the repository's worktrees. `canonical` maps a
/// path to the spelling Git reports (macOS `/tmp` is `/private/tmp`), and
/// `is_dir` checks a relocated directory; both are I/O the caller provides,
/// which keeps this function pure for tests.
pub(crate) fn resolve(
    inputs: &FollowInputs,
    worktrees: &[Worktree],
    canonical: impl Fn(&Path) -> PathBuf,
    is_dir: impl Fn(&Path) -> bool,
) -> Resolution {
    let launch_canonical = canonical(&inputs.launch_cwd);
    let Some(launch) = worktree_for(worktrees, &launch_canonical) else {
        return Resolution::launch_only(inputs.launch_cwd.clone());
    };
    let relative = launch_canonical
        .strip_prefix(&launch.path)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut candidates = vec![Candidate {
        root: launch.path.clone(),
        branch: launch.branch.clone(),
        directory: inputs.launch_cwd.clone(),
        at: inputs.launch_at,
        source: Source::Launch,
        launch: true,
        managed: inputs.managed,
    }];
    // The newest directory signal per candidate, which names where it starts.
    let mut moved_at: Vec<Option<u64>> = vec![None];
    for signal in &inputs.signals {
        let path = canonical(&signal.path);
        let Some(worktree) = worktree_for(worktrees, &path) else {
            continue;
        };
        let index = match candidates.iter().position(|c| c.root == worktree.path) {
            Some(index) => index,
            None => {
                let relocated = worktree.path.join(&relative);
                candidates.push(Candidate {
                    root: worktree.path.clone(),
                    branch: worktree.branch.clone(),
                    directory: if relative.as_os_str().is_empty() || !is_dir(&relocated) {
                        worktree.path.clone()
                    } else {
                        relocated
                    },
                    at: 0,
                    source: Source::Edit,
                    launch: false,
                    managed: false,
                });
                moved_at.push(None);
                candidates.len() - 1
            }
        };
        let candidate = &mut candidates[index];
        if (signal.at, signal.source) > (candidate.at, candidate.source) {
            candidate.at = signal.at;
            candidate.source = signal.source;
        }
        if signal.source.is_directory()
            && !candidate.launch
            && moved_at[index].is_none_or(|at| signal.at >= at)
        {
            moved_at[index] = Some(signal.at);
            candidate.directory = path;
        }
    }
    candidates.sort_by(|left, right| {
        (right.at, right.source, right.launch).cmp(&(left.at, left.source, left.launch))
    });
    Resolution {
        launch_cwd: inputs.launch_cwd.clone(),
        candidates,
    }
}

/// What a background resolution hands back.
#[derive(Debug)]
pub(crate) struct FollowOutcome {
    pub inputs: FollowInputs,
    pub process: Option<Signal>,
    pub resolution: Resolution,
}

/// Blocking: lists the launch repository's worktrees and resolves. Called on
/// a background thread only.
pub(crate) fn resolve_blocking(inputs: FollowInputs) -> FollowOutcome {
    let process = inputs
        .signals
        .iter()
        .find(|signal| signal.source == Source::Process)
        .cloned();
    let resolution = match list_worktrees(&inputs.launch_cwd) {
        Some(worktrees) => resolve(&inputs, &worktrees, canonicalize_lenient, Path::is_dir),
        None => Resolution::launch_only(inputs.launch_cwd.clone()),
    };
    FollowOutcome {
        inputs,
        process,
        resolution,
    }
}

/// `git worktree list --porcelain` for the repository containing `cwd`.
pub(crate) fn list_worktrees(cwd: &Path) -> Option<Vec<Worktree>> {
    let output = git(cwd, &["worktree", "list", "--porcelain"], GIT_LOCAL_TIMEOUT)?;
    Some(parse_worktree_list(&output))
}

/// Canonicalizes the longest existing ancestor and re-appends the rest, so a
/// deleted file still maps into its checkout.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(resolved, |path, component| path.join(component));
        }
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return path.to_path_buf();
        };
        rest.push(name.to_owned());
        existing = parent;
    }
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn millis(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Default-branch base for fresh worktrees and the staleness hint.

const GIT_LOCAL_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The ref a fresh worktree starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseRef {
    /// A revision `git worktree add` accepts: `origin/main`, or `main`.
    pub reference: String,
    /// The branch's short name for labels: `main`.
    pub branch: String,
    /// Whether `reference` is a remote-tracking ref.
    pub remote: bool,
}

/// Picks the default branch. `origin_head` is `git symbolic-ref
/// refs/remotes/origin/HEAD` (`refs/remotes/origin/main`); `exists` checks
/// a short ref. Remote-tracking refs come first: a local `main` is whatever
/// the user last pulled, which is exactly the stale base this avoids.
pub(crate) fn choose_base_ref(
    origin_head: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Option<BaseRef> {
    let head = origin_head
        .map(str::trim)
        .and_then(|head| head.strip_prefix("refs/remotes/"))
        .filter(|head| head.starts_with("origin/") && exists(head));
    if let Some(head) = head {
        return Some(BaseRef {
            branch: head.trim_start_matches("origin/").to_owned(),
            reference: head.to_owned(),
            remote: true,
        });
    }
    for branch in ["main", "master"] {
        let remote = format!("origin/{branch}");
        if exists(&remote) {
            return Some(BaseRef {
                reference: remote,
                branch: branch.to_owned(),
                remote: true,
            });
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|branch| exists(branch))
        .map(|branch| BaseRef {
            reference: branch.to_owned(),
            branch: branch.to_owned(),
            remote: false,
        })
}

/// The default branch as this machine knows it now, without the network.
pub(crate) fn local_base_ref(repo: &Path) -> Option<BaseRef> {
    let head = git(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        GIT_LOCAL_TIMEOUT,
    );
    choose_base_ref(head.as_deref(), |reference| {
        git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
            GIT_LOCAL_TIMEOUT,
        )
        .is_some()
    })
}

/// A fresh base for a new worktree: fetches the default branch first, then
/// resolves. `fetched` is false when the fetch failed (offline, no remote,
/// auth), so the caller can say the base may be behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FreshBase {
    pub base: BaseRef,
    pub fetched: bool,
}

/// Blocking and networked: background threads only.
pub(crate) fn fetch_default_base(repo: &Path) -> Option<FreshBase> {
    let known = local_base_ref(repo);
    let branch = known
        .as_ref()
        .map_or("HEAD", |base| base.branch.as_str())
        .to_owned();
    let fetched = git(
        repo,
        &["fetch", "--quiet", "--no-tags", "origin", &branch],
        GIT_FETCH_TIMEOUT,
    )
    .is_some();
    let base = local_base_ref(repo).or(known)?;
    Some(FreshBase {
        fetched: fetched && base.remote,
        base,
    })
}

/// How far a checkout trails the default branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Staleness {
    pub behind: u32,
    pub base: BaseRef,
}

/// Commits on the default branch that `worktree`'s HEAD lacks, from refs
/// already on disk (no fetch). `None` when unknown or not behind.
pub(crate) fn staleness(worktree: &Path) -> Option<Staleness> {
    let base = local_base_ref(worktree)?;
    let count = git(
        worktree,
        &[
            "rev-list",
            "--count",
            "--end-of-options",
            &format!("HEAD..{}", base.reference),
        ],
        GIT_LOCAL_TIMEOUT,
    )?;
    let behind = parse_count(&count)?;
    (behind > 0).then_some(Staleness { behind, base })
}

pub(crate) fn parse_count(output: &str) -> Option<u32> {
    output.trim().parse().ok()
}

/// Runs a fixed Git command with prompts and optional locks disabled and a
/// deadline. Returns stdout on success.
fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .current_dir(cwd)
        .arg("--no-pager")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?.ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Per-window state the inspector keeps.

/// What the inspector remembers per Session: the last inputs it resolved,
/// the result, the sampled process directory, and the user's pin.
#[derive(Default)]
pub(crate) struct FollowState {
    entries: std::collections::HashMap<SessionId, FollowEntry>,
}

#[derive(Default)]
struct FollowEntry {
    inputs: Option<FollowInputs>,
    resolution: Option<std::sync::Arc<Resolution>>,
    process: Option<Signal>,
    pin: Option<PathBuf>,
    staleness: Option<(PathBuf, Option<Staleness>)>,
}

impl FollowState {
    pub(crate) fn process(&self, id: &SessionId) -> Option<&Signal> {
        self.entries.get(id)?.process.as_ref()
    }

    /// Whether `inputs` differ from the last ones resolved for `id`.
    pub(crate) fn is_stale(&self, id: &SessionId, inputs: &FollowInputs) -> bool {
        self.entries
            .get(id)
            .is_none_or(|entry| entry.inputs.as_ref() != Some(inputs))
    }

    /// Records a shell's live directory synchronously (no I/O needed).
    pub(crate) fn observe_shell(
        &mut self,
        id: &SessionId,
        terminal_cwd: Option<&str>,
        launch_cwd: &Path,
    ) {
        let entry = self.entries.entry(id.clone()).or_default();
        entry.process = observe_process(
            entry.process.as_ref(),
            terminal_cwd.map(PathBuf::from),
            launch_cwd,
            true,
            now_millis(),
        );
    }

    pub(crate) fn finish(&mut self, id: &SessionId, outcome: FollowOutcome) {
        let entry = self.entries.entry(id.clone()).or_default();
        entry.process = outcome.process;
        entry.inputs = Some(outcome.inputs);
        entry.resolution = Some(std::sync::Arc::new(outcome.resolution));
    }

    pub(crate) fn resolution(&self, id: &SessionId) -> Option<&Resolution> {
        self.entries.get(id)?.resolution.as_deref()
    }

    pub(crate) fn pin(&self, id: &SessionId) -> Option<&Path> {
        self.entries.get(id)?.pin.as_deref()
    }

    pub(crate) fn set_pin(&mut self, id: &SessionId, root: Option<PathBuf>) {
        self.entries.entry(id.clone()).or_default().pin = root;
    }

    /// The chosen candidate for a Session whose launch directory is still
    /// `launch_cwd`.
    pub(crate) fn chosen(&self, id: &SessionId, launch_cwd: &Path) -> Option<&Candidate> {
        let entry = self.entries.get(id)?;
        let resolution = entry.resolution.as_deref()?;
        if resolution.launch_cwd != launch_cwd {
            return None;
        }
        resolution.chosen(entry.pin.as_deref())
    }

    /// The directory the panel shows: the chosen candidate's, else `None`
    /// (callers fall back to the launch directory).
    pub(crate) fn directory(&self, id: &SessionId, launch_cwd: &Path) -> Option<PathBuf> {
        self.chosen(id, launch_cwd).map(|c| c.directory.clone())
    }

    /// The cached staleness for `root`, if it was computed for that root.
    pub(crate) fn staleness(&self, id: &SessionId, root: &Path) -> Option<Option<&Staleness>> {
        let (cached, staleness) = self.entries.get(id)?.staleness.as_ref()?;
        (cached == root).then_some(staleness.as_ref())
    }

    pub(crate) fn set_staleness(
        &mut self,
        id: &SessionId,
        root: PathBuf,
        staleness: Option<Staleness>,
    ) {
        self.entries.entry(id.clone()).or_default().staleness = Some((root, staleness));
    }

    pub(crate) fn clear_staleness(&mut self, id: &SessionId) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.staleness = None;
        }
    }
}

// ---------------------------------------------------------------------------
// The inspector's side: scheduling resolutions and the worktree chip.

/// Coalesces a burst of hook updates (an Agent editing several new files)
/// into one `git worktree list`.
const FOLLOW_DEBOUNCE: Duration = Duration::from_millis(150);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(1);

/// The view that owns a [`FollowController`].
pub(crate) trait FollowHost: Sized + 'static {
    fn follow(&mut self) -> &mut FollowController;
    /// A resolution or a pin changed what the panel should show.
    fn follow_changed(&mut self, cx: &mut Context<Self>);
    /// "New agent from main" in the chip.
    fn new_agent_from_default_branch(&mut self, session: SessionId, cx: &mut Context<Self>);
}

#[derive(Default)]
pub(crate) struct FollowController {
    pub state: FollowState,
    /// The resolution in flight, so store churn with unchanged inputs does
    /// not keep restarting its debounce.
    pending: Option<(SessionId, FollowInputs)>,
    generation: u64,
    task: Option<Task<()>>,
    staleness_task: Option<(PathBuf, Task<()>)>,
    /// The session whose checkout menu is open; another session's chip
    /// shows closed.
    menu_open: Option<SessionId>,
}

impl FollowController {
    /// The directory the panel shows for `session`: the followed checkout,
    /// else the launch directory.
    pub(crate) fn directory_for(&self, session: &SessionRecord) -> PathBuf {
        let launch = PathBuf::from(&session.cwd);
        if session.host.is_some() {
            return launch;
        }
        self.state.directory(&session.id, &launch).unwrap_or(launch)
    }

    /// A reveal re-reads what may have changed while hidden.
    pub(crate) fn forget_staleness(&mut self, id: &SessionId) {
        self.state.clear_staleness(id);
        self.staleness_task = None;
    }
}

/// Schedules a resolution for `session` when its inputs changed, or always
/// with `sample_process` (a reveal or a selection change), which also reads
/// an Agent process's live directory once.
pub(crate) fn sync<T: FollowHost>(
    host: &mut T,
    session: &SessionRecord,
    sample_process: bool,
    runtime: &Arc<StoreRuntime>,
    tokio: &tokio::runtime::Handle,
    cx: &mut Context<T>,
) {
    let id = session.id.clone();
    let shell = session.kind == AgentKind::SHELL;
    let follow = host.follow();
    if shell {
        follow.state.observe_shell(
            &id,
            session.terminal_cwd.as_deref(),
            Path::new(&session.cwd),
        );
    }
    let Some(inputs) = FollowInputs::of(session, follow.state.process(&id)) else {
        return;
    };
    let sample_process = sample_process && !shell;
    if !sample_process
        && (!follow.state.is_stale(&id, &inputs)
            || follow
                .pending
                .as_ref()
                .is_some_and(|(pending, queued)| *pending == id && *queued == inputs))
    {
        return;
    }
    follow.generation = follow.generation.wrapping_add(1);
    let generation = follow.generation;
    follow.pending = Some((id.clone(), inputs.clone()));
    let previous = follow.state.process(&id).cloned();
    let client = Arc::clone(runtime.client());
    let tokio = tokio.clone();
    follow.task = Some(cx.spawn(async move |this, cx| {
        cx.background_executor().timer(FOLLOW_DEBOUNCE).await;
        let mut inputs = inputs;
        if sample_process {
            let session_id = id.clone();
            let sampled = tokio
                .spawn(async move {
                    tokio::time::timeout(PROCESS_TIMEOUT, client.process_info(&session_id))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .and_then(|info| match info.process.working_directory {
                            diri_proto::process_facts::ProcessValue::Available { value } => {
                                Some(PathBuf::from(value))
                            }
                            diri_proto::process_facts::ProcessValue::Unavailable { .. } => None,
                        })
                })
                .await
                .ok()
                .flatten();
            inputs
                .signals
                .retain(|signal| signal.source != Source::Process);
            inputs.signals.extend(observe_process(
                previous.as_ref(),
                sampled,
                &inputs.launch_cwd,
                false,
                now_millis(),
            ));
        }
        let outcome = cx
            .background_spawn(async move { resolve_blocking(inputs) })
            .await;
        let _ = this.update(cx, |this, cx| {
            let follow = this.follow();
            if follow.generation != generation {
                return;
            }
            follow.pending = None;
            follow.state.finish(&id, outcome);
            this.follow_changed(cx);
        });
    }));
}

/// Measures how far the shown checkout trails the default branch, once per
/// root until [`FollowController::forget_staleness`]. Local refs only.
pub(crate) fn refresh_staleness<T: FollowHost>(
    host: &mut T,
    id: &SessionId,
    root: PathBuf,
    cx: &mut Context<T>,
) {
    let follow = host.follow();
    if follow.state.staleness(id, &root).is_some()
        || follow
            .staleness_task
            .as_ref()
            .is_some_and(|(pending, _)| *pending == root)
    {
        return;
    }
    let id = id.clone();
    let path = root.clone();
    let task = cx.spawn(async move |this, cx| {
        let measured = cx
            .background_spawn({
                let path = path.clone();
                async move { staleness(&path) }
            })
            .await;
        let _ = this.update(cx, |this, cx| {
            let follow = this.follow();
            follow.staleness_task = None;
            follow.state.set_staleness(&id, path, measured);
            cx.notify();
        });
    });
    host.follow().staleness_task = Some((root, task));
}

/// The worktree chip under the panel's tab strip: which checkout the panel
/// shows, a menu to switch when the Agent touched several, and a quiet
/// "behind main" hint. Absent when there is nothing to say: one checkout,
/// the launch one, and current.
pub(crate) fn render_bar<T: FollowHost>(
    controller: &FollowController,
    session: Option<&SessionRecord>,
    colors: SemanticColors,
    cx: &mut Context<T>,
) -> Option<AnyElement> {
    let session = session.filter(|session| session.host.is_none())?;
    let launch = PathBuf::from(&session.cwd);
    let resolution = controller.state.resolution(&session.id)?;
    if resolution.launch_cwd != launch {
        return None;
    }
    let pin = controller.state.pin(&session.id);
    let chosen = resolution.chosen(pin)?;
    let staleness = controller
        .state
        .staleness(&session.id, &chosen.root)
        .flatten()
        .cloned();
    let several = resolution.candidates.len() > 1;
    if !several && chosen.launch && staleness.is_none() {
        return None;
    }
    let label = candidate_label(chosen);
    let detail = if pin.is_some() {
        "Pinned"
    } else if chosen.launch {
        "Launch checkout"
    } else {
        "Following agent"
    };
    let menu_open = controller.menu_open.as_ref() == Some(&session.id) && several;
    let chip = div()
        .id("inspector-worktree-chip")
        .debug_selector(|| "INSPECTOR_WORKTREE_CHIP".to_owned())
        .min_w(px(0.0))
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .rounded(px(Radius::BADGE))
        .bg(colors.primary.alpha(if menu_open { 0.09 } else { 0.0 }))
        .when(several, |chip| {
            let id = session.id.clone();
            chip.cursor_pointer()
                .hover(move |chip| chip.bg(colors.primary.alpha(0.07)))
                .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                    let follow = this.follow();
                    follow.menu_open = if menu_open { None } else { Some(id.clone()) };
                    cx.notify();
                    cx.stop_propagation();
                }))
        })
        .child(sf_symbol("arrow.triangle.branch", 10.5, colors.tertiary))
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .font_family(crate::fonts::mono_family())
                .text_size(px(Typo::META_MONO.size))
                .text_color(colors.secondary)
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(Typo::META.size))
                .text_color(colors.tertiary)
                .child(detail),
        )
        .when(several, |chip| {
            chip.child(sf_symbol("chevron.down", 8.5, colors.tertiary))
        });
    let hint = staleness.map(|staleness| {
        let id = session.id.clone();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(colors.tertiary)
                    .child(format!(
                        "{} behind {}",
                        staleness.behind, staleness.base.branch
                    )),
            )
            .child(
                div()
                    .id("inspector-new-agent-from-default")
                    .debug_selector(|| "INSPECTOR_NEW_AGENT_FROM_DEFAULT".to_owned())
                    .h(px(20.0))
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::BADGE))
                    .text_size(px(Typo::META.size))
                    .text_color(colors.secondary)
                    .bg(colors.primary.alpha(0.05))
                    .cursor_pointer()
                    .hover(move |button| button.bg(colors.primary.alpha(0.10)))
                    .child(format!("New agent from {}", staleness.base.branch))
                    .on_click(cx.listener(move |this: &mut T, _, _, cx| {
                        this.new_agent_from_default_branch(id.clone(), cx);
                        cx.stop_propagation();
                    })),
            )
    });
    let bar = div()
        .h(px(28.0))
        .flex_none()
        .px(px(6.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .border_b_1()
        .border_color(colors.primary.alpha(0.06))
        .child(chip)
        .when_some(hint, |bar, hint| bar.child(hint));
    let menu = menu_open.then(|| {
        let rows = div().py(px(4.0)).children(
            std::iter::once(menu_row::<T>(
                "follow".into(),
                "Follow the agent".into(),
                "Show the checkout it worked in last".into(),
                pin.is_none(),
                None,
                colors,
                session.id.clone(),
                cx,
            ))
            .chain(resolution.candidates.iter().enumerate().map(
                |(index, candidate)| {
                    menu_row::<T>(
                        format!("candidate-{index}").into(),
                        candidate_label(candidate),
                        candidate_detail(candidate),
                        pin.is_some_and(|pin| pin == candidate.root),
                        Some(candidate.root.clone()),
                        colors,
                        session.id.clone(),
                        cx,
                    )
                },
            )),
        );
        div()
            .id("inspector-worktree-menu")
            .absolute()
            .top(px(26.0))
            .left(px(6.0))
            .w(px(280.0))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|this: &mut T, _, _, cx| {
                this.follow().menu_open = None;
                cx.notify();
            }))
            .child(FloatingSurface::new(colors, rows).radius(Radius::FLOATING_MENU))
    });
    Some(
        div()
            .relative()
            .flex_none()
            .child(bar)
            .when_some(menu, |wrapper, menu| wrapper.child(deferred(menu)))
            .into_any_element(),
    )
}

#[allow(clippy::too_many_arguments)]
fn menu_row<T: FollowHost>(
    id: SharedString,
    title: SharedString,
    detail: SharedString,
    selected: bool,
    pin: Option<PathBuf>,
    colors: SemanticColors,
    session: SessionId,
    cx: &mut Context<T>,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("worktree-option-{id}")))
        .min_h(px(40.0))
        .px(px(10.0))
        .py(px(5.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .cursor_pointer()
        .hover(move |row| row.bg(colors.primary.alpha(0.09)))
        .child(
            div()
                .w(px(14.0))
                .flex_none()
                .flex()
                .justify_center()
                .when(selected, |slot| {
                    slot.child(sf_symbol("checkmark", 10.5, colors.primary))
                }),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::ROW.size))
                        .text_color(colors.primary)
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(colors.tertiary)
                        .child(detail),
                ),
        )
        .on_click(cx.listener(move |this: &mut T, _, _, cx| {
            let follow = this.follow();
            follow.state.set_pin(&session, pin.clone());
            follow.menu_open = None;
            this.follow_changed(cx);
            cx.stop_propagation();
        }))
        .into_any_element()
}

fn candidate_label(candidate: &Candidate) -> SharedString {
    candidate
        .branch
        .clone()
        .or_else(|| {
            candidate
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| candidate.root.to_string_lossy().into_owned())
        .into()
}

fn candidate_detail(candidate: &Candidate) -> SharedString {
    let folder = candidate.root.file_name().map_or_else(
        || candidate.root.to_string_lossy(),
        |name| name.to_string_lossy(),
    );
    let what = match (candidate.launch, candidate.managed, candidate.source) {
        (true, true, Source::Launch) => "Diri worktree".to_owned(),
        (true, false, Source::Launch) => "Launch checkout".to_owned(),
        (_, _, Source::Hook) => format!("Agent moved here {}", ago(candidate.at)),
        (_, _, Source::Process) => format!("Working here {}", ago(candidate.at)),
        (_, _, Source::Edit) => format!("Edited {}", ago(candidate.at)),
        (false, _, Source::Launch) => String::new(),
    };
    format!("{folder} · {what}").into()
}

fn ago(at: u64) -> String {
    let seconds = now_millis().saturating_sub(at) / 1000;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/.claude/worktrees/fix
HEAD 2222222222222222222222222222222222222222
branch refs/heads/claude/fix
locked

worktree /repo-feature
HEAD 3333333333333333333333333333333333333333
detached

worktree /bare.git
bare
";

    fn worktrees() -> Vec<Worktree> {
        parse_worktree_list(LIST)
    }

    fn inputs(launch: &str, signals: Vec<Signal>) -> FollowInputs {
        FollowInputs {
            launch_cwd: PathBuf::from(launch),
            launch_at: 100,
            managed: false,
            shell: false,
            signals,
        }
    }

    fn signal(path: &str, at: u64, source: Source) -> Signal {
        Signal {
            path: PathBuf::from(path),
            at,
            source,
        }
    }

    fn resolve_plain(inputs: &FollowInputs) -> Resolution {
        resolve(inputs, &worktrees(), Path::to_path_buf, |_| true)
    }

    fn roots(resolution: &Resolution) -> Vec<&str> {
        resolution
            .candidates
            .iter()
            .map(|c| c.root.to_str().unwrap())
            .collect()
    }

    #[test]
    fn porcelain_lists_branches_detached_and_bare_checkouts() {
        assert_eq!(
            worktrees(),
            vec![
                Worktree {
                    path: "/repo".into(),
                    branch: Some("main".into()),
                    bare: false
                },
                Worktree {
                    path: "/repo/.claude/worktrees/fix".into(),
                    branch: Some("claude/fix".into()),
                    bare: false
                },
                Worktree {
                    path: "/repo-feature".into(),
                    branch: None,
                    bare: false
                },
                Worktree {
                    path: "/bare.git".into(),
                    branch: None,
                    bare: true
                },
            ]
        );
        assert!(parse_worktree_list("").is_empty());
        assert!(parse_worktree_list("garbage\nbranch refs/heads/x\n").is_empty());
    }

    #[test]
    fn a_path_maps_to_its_deepest_checkout_by_component() {
        let list = worktrees();
        let root = |path: &str| worktree_for(&list, Path::new(path)).map(|w| w.path.clone());
        assert_eq!(root("/repo/src/lib.rs"), Some("/repo".into()));
        assert_eq!(
            root("/repo/.claude/worktrees/fix/src/lib.rs"),
            Some("/repo/.claude/worktrees/fix".into())
        );
        // A sibling whose name extends the main checkout's is not inside it.
        assert_eq!(root("/repo-feature/a"), Some("/repo-feature".into()));
        assert_eq!(root("/repository/a"), None);
        assert_eq!(root("/bare.git/objects"), None);
    }

    #[test]
    fn without_evidence_the_launch_directory_stays() {
        let resolution = resolve_plain(&inputs("/repo/crates/app", Vec::new()));
        assert_eq!(roots(&resolution), ["/repo"]);
        let chosen = resolution.chosen(None).unwrap();
        assert!(chosen.launch);
        assert_eq!(chosen.directory, Path::new("/repo/crates/app"));
    }

    #[test]
    fn outside_git_there_is_nothing_to_follow() {
        let resolution = resolve_plain(&inputs(
            "/elsewhere",
            vec![signal("/repo", 500, Source::Hook)],
        ));
        assert!(resolution.candidates.is_empty());
        assert!(resolution.chosen(None).is_none());
    }

    #[test]
    fn a_hook_reported_move_into_a_worktree_is_followed() {
        let resolution = resolve_plain(&inputs(
            "/repo",
            vec![signal("/repo/.claude/worktrees/fix", 500, Source::Hook)],
        ));
        let chosen = resolution.chosen(None).unwrap();
        assert_eq!(chosen.root, Path::new("/repo/.claude/worktrees/fix"));
        assert_eq!(chosen.branch.as_deref(), Some("claude/fix"));
        assert_eq!(chosen.source, Source::Hook);
        assert!(!chosen.launch);
    }

    #[test]
    fn the_most_recent_activity_wins_and_ties_go_to_stronger_evidence() {
        // Moved into the fix worktree, then edited the sibling checkout by
        // absolute path without moving: the newer edit wins.
        let resolution = resolve_plain(&inputs(
            "/repo",
            vec![
                signal("/repo/.claude/worktrees/fix", 500, Source::Hook),
                signal("/repo-feature/src/a.rs", 900, Source::Edit),
                signal("/repo/.claude/worktrees/fix/b.rs", 600, Source::Edit),
            ],
        ));
        assert_eq!(
            roots(&resolution),
            ["/repo-feature", "/repo/.claude/worktrees/fix", "/repo"]
        );
        // Same instant: the reported move outranks the edit.
        let tie = resolve_plain(&inputs(
            "/repo",
            vec![
                signal("/repo-feature/src/a.rs", 700, Source::Edit),
                signal("/repo/.claude/worktrees/fix", 700, Source::Hook),
            ],
        ));
        assert_eq!(
            tie.chosen(None).unwrap().root,
            Path::new("/repo/.claude/worktrees/fix")
        );
        // Returning to the launch checkout makes it current again.
        let back = resolve_plain(&inputs(
            "/repo/crates",
            vec![
                signal("/repo-feature/src/a.rs", 700, Source::Edit),
                signal("/repo", 800, Source::Hook),
            ],
        ));
        let chosen = back.chosen(None).unwrap();
        assert!(chosen.launch);
        // The launch checkout keeps its launch directory, never the hook's.
        assert_eq!(chosen.directory, Path::new("/repo/crates"));
    }

    #[test]
    fn evidence_outside_the_repository_is_ignored() {
        let resolution = resolve_plain(&inputs(
            "/repo",
            vec![
                signal("/tmp/scratch.txt", 900, Source::Edit),
                signal("/Users/me/.claude/settings.json", 950, Source::Edit),
            ],
        ));
        assert_eq!(roots(&resolution), ["/repo"]);
    }

    #[test]
    fn a_new_checkout_starts_where_the_agent_moved_or_the_relocated_launch_subdirectory() {
        let moved = resolve_plain(&inputs(
            "/repo/crates/app",
            vec![signal("/repo-feature/crates", 900, Source::Hook)],
        ));
        assert_eq!(
            moved.chosen(None).unwrap().directory,
            Path::new("/repo-feature/crates")
        );
        let edited = resolve(
            &inputs(
                "/repo/crates/app",
                vec![signal("/repo-feature/x.rs", 900, Source::Edit)],
            ),
            &worktrees(),
            Path::to_path_buf,
            |path| path == Path::new("/repo-feature/crates/app"),
        );
        assert_eq!(
            edited.chosen(None).unwrap().directory,
            Path::new("/repo-feature/crates/app")
        );
        let missing = resolve_plain(&inputs(
            "/repo",
            vec![signal("/repo-feature/x.rs", 900, Source::Edit)],
        ));
        assert_eq!(
            missing.chosen(None).unwrap().directory,
            Path::new("/repo-feature")
        );
    }

    #[test]
    fn a_pin_overrides_recency_until_the_checkout_disappears() {
        let resolution = resolve_plain(&inputs(
            "/repo",
            vec![signal("/repo-feature/x.rs", 900, Source::Edit)],
        ));
        assert_eq!(
            resolution.chosen(Some(Path::new("/repo"))).unwrap().root,
            Path::new("/repo")
        );
        assert_eq!(
            resolution.chosen(Some(Path::new("/gone"))).unwrap().root,
            Path::new("/repo-feature")
        );
    }

    #[test]
    fn canonical_spellings_are_matched() {
        let list = parse_worktree_list(
            "worktree /private/tmp/repo\nbranch refs/heads/main\n\nworktree /private/tmp/repo-x\nbranch refs/heads/x\n",
        );
        let canonical = |path: &Path| {
            path.strip_prefix("/tmp")
                .map(|rest| Path::new("/private/tmp").join(rest))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let resolution = resolve(
            &inputs(
                "/tmp/repo",
                vec![signal("/tmp/repo-x/a", 900, Source::Edit)],
            ),
            &list,
            canonical,
            |_| true,
        );
        assert_eq!(
            resolution.chosen(None).unwrap().branch.as_deref(),
            Some("x")
        );
        // The launch candidate keeps the record's own spelling.
        assert_eq!(
            resolution.candidates.last().unwrap().directory,
            Path::new("/tmp/repo")
        );
    }

    #[test]
    fn an_agent_process_that_never_moved_is_not_evidence() {
        let launch = Path::new("/repo");
        assert_eq!(
            observe_process(None, Some("/repo".into()), launch, false, 5),
            None
        );
        let moved = observe_process(None, Some("/repo-feature".into()), launch, false, 5).unwrap();
        assert_eq!(moved.at, 5);
        // The first-seen time survives later samples of the same directory.
        let again = observe_process(Some(&moved), Some("/repo-feature".into()), launch, false, 9);
        assert_eq!(again.unwrap().at, 5);
        // A shell's directory is evidence even at its launch directory.
        assert!(observe_process(None, Some("/repo".into()), launch, true, 5).is_some());
        assert_eq!(observe_process(Some(&moved), None, launch, false, 9), None);
    }

    #[test]
    fn inputs_come_from_the_record_and_skip_remote_sessions() {
        let mut session = crate::notes::work_item_tests::record("s", AgentKind::CLAUDE_CODE);
        session.cwd = "/repo".into();
        session.created_at = diri_proto::DateMillis(1000.0);
        session.agent_workspace = Some(diri_proto::AgentWorkspace {
            cwd: Some(diri_proto::AgentPlace {
                path: "/repo-feature".into(),
                at: diri_proto::DateMillis(2000.0),
            }),
            edits: vec![diri_proto::AgentPlace {
                path: "/repo-feature/a.rs".into(),
                at: diri_proto::DateMillis(3000.0),
            }],
        });
        let inputs = FollowInputs::of(&session, None).unwrap();
        assert_eq!(inputs.launch_at, 1000);
        assert_eq!(
            inputs.signals,
            vec![
                signal("/repo-feature", 2000, Source::Hook),
                signal("/repo-feature/a.rs", 3000, Source::Edit)
            ]
        );
        session.host = Some("forge".into());
        assert!(FollowInputs::of(&session, None).is_none());
    }

    #[test]
    fn the_default_branch_prefers_remote_tracking_refs() {
        let refs: &'static [&str] = &[
            "origin/trunk",
            "origin/main",
            "origin/master",
            "main",
            "master",
        ];
        let exists = |available: &'static [&'static str]| move |r: &str| available.contains(&r);
        assert_eq!(
            choose_base_ref(Some("refs/remotes/origin/trunk\n"), exists(refs)),
            Some(BaseRef {
                reference: "origin/trunk".into(),
                branch: "trunk".into(),
                remote: true
            })
        );
        // origin/HEAD unset (common after `git init` + `remote add`).
        assert_eq!(
            choose_base_ref(None, exists(refs)).unwrap().reference,
            "origin/main"
        );
        assert_eq!(
            choose_base_ref(None, exists(&["origin/master", "main"]))
                .unwrap()
                .reference,
            "origin/master"
        );
        // A dangling origin/HEAD falls through to the usual names.
        assert_eq!(
            choose_base_ref(Some("refs/remotes/origin/gone"), exists(&["origin/main"]))
                .unwrap()
                .reference,
            "origin/main"
        );
        // No remote at all: the local default branch, flagged as local.
        let local = choose_base_ref(None, exists(&["master"])).unwrap();
        assert_eq!((local.reference.as_str(), local.remote), ("master", false));
        assert_eq!(choose_base_ref(None, exists(&[])), None);
        assert_eq!(parse_count(" 377\n"), Some(377));
        assert_eq!(parse_count("x"), None);
    }

    /// A real repository: `git worktree add` creates a sibling checkout, an
    /// edit there is followed, and the default-branch base and staleness come
    /// from the remote-tracking ref rather than the stale local branch.
    #[test]
    fn follows_a_real_git_worktree_and_measures_staleness() {
        let Some(repo) = TempRepo::new() else {
            return;
        };
        let main = repo.path.join("main");
        std::fs::create_dir(&main).unwrap();
        repo.git(&main, &["init", "--quiet"]);
        repo.git(&main, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        repo.git(&main, &["add", "a.txt"]);
        repo.git(&main, &["commit", "--quiet", "-m", "first"]);
        let feature = repo.path.join("main-feature");
        repo.git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                feature.to_str().unwrap(),
            ],
        );
        std::fs::write(feature.join("b.txt"), "b").unwrap();

        let list = list_worktrees(&main).expect("worktree list");
        assert_eq!(list.len(), 2);
        let launch_at = 1;
        let outcome = resolve_blocking(FollowInputs {
            launch_cwd: main.clone(),
            launch_at,
            managed: false,
            shell: false,
            signals: vec![Signal {
                path: feature.join("b.txt"),
                at: 10,
                source: Source::Edit,
            }],
        });
        let chosen = outcome.resolution.chosen(None).expect("chosen");
        assert_eq!(chosen.branch.as_deref(), Some("feature"));
        assert_eq!(chosen.root, std::fs::canonicalize(&feature).unwrap());
        assert_eq!(outcome.resolution.candidates.len(), 2);

        // Not behind anything yet; then origin/main moves ahead of local main.
        assert_eq!(staleness(&main), None);
        let head = repo.git(&main, &["rev-parse", "HEAD"]);
        repo.git(
            &main,
            &["update-ref", "refs/remotes/origin/main", head.trim()],
        );
        std::fs::write(main.join("c.txt"), "c").unwrap();
        repo.git(&main, &["add", "c.txt"]);
        repo.git(&main, &["commit", "--quiet", "-m", "second"]);
        let ahead = repo.git(&main, &["rev-parse", "HEAD"]);
        repo.git(
            &main,
            &["update-ref", "refs/remotes/origin/main", ahead.trim()],
        );
        repo.git(&main, &["reset", "--quiet", "--hard", head.trim()]);
        let stale = staleness(&main).expect("behind origin/main");
        assert_eq!(stale.behind, 1);
        assert_eq!(stale.base.reference, "origin/main");
        assert_eq!(stale.base.branch, "main");
        // Offline: no `origin` remote to fetch from, so the fetch fails and
        // the on-disk remote-tracking ref is used and reported as unfetched.
        let fresh = fetch_default_base(&main).expect("base");
        assert_eq!(fresh.base.reference, "origin/main");
        assert!(!fresh.fetched);
    }

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Option<Self> {
            if !Command::new("git")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
            {
                eprintln!("skipping worktree follow test: git is unavailable");
                return None;
            }
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "diri-workspace-follow-{}-{ordinal}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).ok()?;
            Some(Self { path })
        }

        fn git(&self, cwd: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .stdin(Stdio::null())
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Diri Test")
                .env("GIT_AUTHOR_EMAIL", "diri@example.invalid")
                .env("GIT_COMMITTER_NAME", "Diri Test")
                .env("GIT_COMMITTER_EMAIL", "diri@example.invalid")
                .output()
                .expect("run test git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
