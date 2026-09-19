//! Immutable, local completed-terminal storage primitives. Not wired to lifecycle
//! or the desktop yet. The caller must pin the current run under Registry, perform
//! storage work outside that lock, and revalidate the run before publishing a read.
//! No raw replay, process observation, input, notifications, or Holder launch occurs.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use diri_proto::grid::{GridRowCodec, GridUpdate, MAX_GRID_METADATA_BYTES, RowMetadata};
use diri_proto::{DateMillis, ExitInfo, ExitReason, SessionId, SessionRecord, SessionStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkpoint::ScreenCheckpoint;
use crate::holder::HolderStat;

const MAGIC: &[u8; 8] = b"DIRICMP1";
const HEADER_BYTES: usize = 20;
const MAX_METADATA: usize = 4096;
pub const MAX_CHECKPOINT_BYTES: usize = 16 << 20;
pub const MAX_RETAINED_CELLS: usize = 1 << 20;
const MAX_HISTORY_ROWS: usize = 10_000;
/// Retention bounds for the whole directory. Orphans go first; beyond these,
/// the oldest bound artifacts are evicted and their records lose retained
/// output explicitly rather than the disk growing without limit.
pub const MAX_RETAINED_ARTIFACTS: usize = 256;
pub const MAX_RETAINED_BYTES: u64 = 256 << 20;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub enum StorageError {
    UnsupportedRecord,
    IdentityMismatch,
    Corrupt,
    TooLarge,
    Busy,
    Io(io::Error),
}
impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedRecord => {
                f.write_str("completed terminal requires a supported local exit")
            }
            Self::IdentityMismatch => {
                f.write_str("completed terminal does not match the expected run")
            }
            Self::Corrupt => f.write_str("invalid completed terminal artifact"),
            Self::TooLarge => f.write_str("completed terminal exceeds storage limits"),
            Self::Busy => f.write_str("completed terminal storage is busy"),
            Self::Io(error) => write!(f, "completed terminal storage: {error}"),
        }
    }
}
impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for StorageError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
type Result<T> = std::result::Result<T, StorageError>;

/// Captured while the actual local Holder still verifies its owned child. A PID,
/// record timestamp, or raw-log filename cannot substitute for this binding.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletedRunKey {
    session_id: SessionId,
    record_created_at: DateMillis,
    child: diri_proto::process::ProcessIdentity,
    epoch_offset: u64,
}
impl CompletedRunKey {
    pub fn capture(record: &SessionRecord, stat: &HolderStat) -> Result<Self> {
        if record.host.is_some() || !stat.alive {
            return Err(StorageError::UnsupportedRecord);
        }
        if !record.created_at.0.is_finite() || record.id.0.len() > 256 {
            return Err(StorageError::Corrupt);
        }
        let child = stat
            .verified_child_identity()
            .ok_or(StorageError::IdentityMismatch)?;
        let epoch_offset = stat.epoch_offset.ok_or(StorageError::IdentityMismatch)?;
        if epoch_offset > stat.log_offset {
            return Err(StorageError::IdentityMismatch);
        }
        Ok(Self {
            session_id: record.id.clone(),
            record_created_at: record.created_at,
            child,
            epoch_offset,
        })
    }

    /// Binds a record to a run whose identity the Engine captured from an
    /// alive, verified Holder stat at launch or adoption and retained in
    /// memory. This is the same evidence `capture` demands, kept past the
    /// point where the Holder can still be asked.
    pub fn bind(
        record: &SessionRecord,
        child: diri_proto::process::ProcessIdentity,
        epoch_offset: u64,
    ) -> Result<Self> {
        if record.host.is_some() {
            return Err(StorageError::UnsupportedRecord);
        }
        if !record.created_at.0.is_finite() || record.id.0.len() > 256 {
            return Err(StorageError::Corrupt);
        }
        Ok(Self {
            session_id: record.id.clone(),
            record_created_at: record.created_at,
            child,
            epoch_offset,
        })
    }

    /// The verified run this key binds, for re-seeding in-memory bindings.
    pub fn run(&self) -> (diri_proto::process::ProcessIdentity, u64) {
        (self.child, self.epoch_offset)
    }

    /// The artifact file name this exact run publishes to.
    pub fn artifact_name(&self) -> Result<String> {
        Ok(self.name()?.to_string_lossy().into_owned())
    }

    /// Stable identity of this exact run's artifact, distinct from any live
    /// Session owner so retained Find captures never collide with live ones.
    pub fn owner_id(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        format!("completed-{}", digest_hex(&bytes))
    }

    pub fn is_run(&self, child: diri_proto::process::ProcessIdentity, epoch_offset: u64) -> bool {
        self.child == child && self.epoch_offset == epoch_offset
    }

    fn check_record(&self, record: &SessionRecord) -> Result<()> {
        if record.host.is_some() {
            return Err(StorageError::UnsupportedRecord);
        }
        if self.session_id != record.id || self.record_created_at != record.created_at {
            return Err(StorageError::IdentityMismatch);
        }
        Ok(())
    }

    fn name(&self) -> Result<CString> {
        let bytes = serde_json::to_vec(self).map_err(|_| StorageError::Corrupt)?;
        Ok(CString::new(format!("completed-{}.bin", digest_hex(&bytes))).expect("hex filename"))
    }
}

/// Retained visible grid and bounded history only. This does not contain the
/// entire parser, inactive screen, selection, input receipts, or full transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletedCoverage {
    RetainedGridAndHistory,
}

pub struct CompletedTerminal {
    pub coverage: CompletedCoverage,
    pub checkpoint: ScreenCheckpoint,
    pub exit: ExitInfo,
}

impl CompletedTerminal {
    /// A read-only emulator holding the retained grid and history. It accepts
    /// no input and answers no queries; replies it would owe are discarded.
    pub fn screen(&self) -> Option<diri_terminal_state::HeadlessScreen> {
        let checkpoint = &self.checkpoint;
        let mut screen = diri_terminal_state::HeadlessScreen::new(
            usize::from(checkpoint.grid.cols),
            usize::from(checkpoint.grid.rows),
        );
        if !screen.restore(
            &checkpoint.history,
            &checkpoint.grid,
            checkpoint.alt_screen,
            checkpoint.bracketed_paste,
            checkpoint.mouse,
        ) {
            return None;
        }
        screen.restore_history_metadata(&checkpoint.history_metadata);
        let _ = screen.take_replies();
        Some(screen)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    version: u32,
    key: CompletedRunKey,
    exit: ExitInfo,
    checkpoint_sha256: [u8; 32],
    log_offset: u64,
    history_rows: u32,
    alt_screen: bool,
    bracketed_paste: bool,
    mouse: diri_proto::terminal::MouseModes,
    keyboard: Option<diri_proto::terminal_input::KeyboardState>,
}

struct Admission;
impl Admission {
    fn acquire() -> Result<Self> {
        ACTIVE
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .map(|_| Self)
            .map_err(|_| StorageError::Busy)
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Directory is supplied by Engine configuration, not a control caller. It must
/// already be a private, owned directory; neither opening nor loading creates it.
pub struct CompletedTerminalStore {
    directory: File,
    path: std::path::PathBuf,
}

/// What one retention pass did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionReport {
    pub removed_orphans: usize,
    pub evicted: usize,
    pub retained: usize,
    pub retained_bytes: u64,
}

impl CompletedTerminalStore {
    pub fn open(directory: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory)?;
        let metadata = file.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(StorageError::Corrupt);
        }
        Ok(Self {
            directory: file,
            path: directory.to_path_buf(),
        })
    }

    /// Removes every artifact no record binds, then evicts the oldest bound
    /// artifacts until the directory fits the retention bounds. `keep` holds
    /// the artifact names of every run some record still binds; eviction
    /// beyond the bounds is by artifact age, oldest first.
    pub fn retain(&self, keep: &std::collections::HashSet<String>) -> Result<RetentionReport> {
        self.retain_within(keep, MAX_RETAINED_ARTIFACTS, MAX_RETAINED_BYTES)
    }

    pub fn retain_within(
        &self,
        keep: &std::collections::HashSet<String>,
        max_artifacts: usize,
        max_bytes: u64,
    ) -> Result<RetentionReport> {
        let _admission = Admission::acquire()?;
        let mut report = RetentionReport::default();
        let mut bound = Vec::new();
        for entry in std::fs::read_dir(&self.path)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().map(str::to_owned) else {
                continue;
            };
            if !is_artifact_name(&name) {
                continue; // nonce files and anything foreign are not ours to judge
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if !metadata.is_file() {
                continue;
            }
            if keep.contains(&name) {
                let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
                bound.push((modified, metadata.len(), name));
            } else {
                self.unlink(&name)?;
                report.removed_orphans += 1;
            }
        }
        bound.sort();
        let mut total: u64 = bound.iter().map(|(_, len, _)| *len).sum();
        let mut index = 0;
        while index < bound.len() && (bound.len() - index > max_artifacts || total > max_bytes) {
            let (_, len, name) = &bound[index];
            self.unlink(name)?;
            total -= len;
            report.evicted += 1;
            index += 1;
        }
        report.retained = bound.len() - index;
        report.retained_bytes = total;
        Ok(report)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let name = CString::new(name).map_err(|_| StorageError::Corrupt)?;
        // SAFETY: the owned directory fd and NUL-terminated name remain live.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Publish once after observed child exit AND complete PTY drain. Caller
    /// supplies its captured run; no status or final-drain fact is inferred here.
    /// Existing artifacts are never replaced, including on failed relaunch.
    pub fn publish(
        &self,
        record: &SessionRecord,
        key: &CompletedRunKey,
        checkpoint: &ScreenCheckpoint,
        exit: &ExitInfo,
    ) -> Result<()> {
        let _admission = Admission::acquire()?;
        key.check_record(record)?;
        check_exit(record, exit)?;
        if checkpoint.log_offset < key.epoch_offset || !checkpoint.marker_buffer.is_empty() {
            return Err(StorageError::Corrupt);
        }
        check_capture_size(checkpoint)?;
        let mut payload = LimitedBytes(Vec::new());
        let grid = checkpoint
            .grid
            .encode()
            .map_err(|_| StorageError::Corrupt)?;
        preflight_grid(&grid, checkpoint.history.len())?;
        append_section(&mut payload, &grid)?;
        let history =
            GridRowCodec::encode_rows(&checkpoint.history).map_err(|_| StorageError::Corrupt)?;
        preflight_history(
            &history,
            checkpoint.history.len(),
            checkpoint.grid.cols as usize,
        )?;
        append_section(&mut payload, &history)?;
        let annotations =
            serde_json::to_vec(&checkpoint.history_metadata).map_err(|_| StorageError::Corrupt)?;
        append_section(&mut payload, &annotations)?;
        let keyboard = checkpoint
            .keyboard_snapshot
            .as_ref()
            .map(|snapshot| snapshot.encode());
        let snapshot_flags = checkpoint
            .keyboard_snapshot
            .as_ref()
            .map(|snapshot| snapshot.current());
        if checkpoint
            .keyboard
            .and_then(|keyboard| keyboard.enhancements)
            .map(|flags| flags.bits())
            != snapshot_flags
        {
            return Err(StorageError::Corrupt);
        }
        append_section(&mut payload, keyboard.as_deref().unwrap_or_default())?;
        let metadata = Metadata {
            version: 1,
            key: key.clone(),
            exit: exit.clone(),
            checkpoint_sha256: Sha256::digest(&payload.0).into(),
            log_offset: checkpoint.log_offset,
            history_rows: checkpoint.history.len() as u32,
            alt_screen: checkpoint.alt_screen,
            bracketed_paste: checkpoint.bracketed_paste,
            mouse: checkpoint.mouse,
            keyboard: checkpoint.keyboard,
        };
        let metadata = serde_json::to_vec(&metadata).map_err(|_| StorageError::Corrupt)?;
        if metadata.len() > MAX_METADATA {
            return Err(StorageError::TooLarge);
        }
        let name = key.name()?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let nonce =
            CString::new(format!(".completed-{}.tmp", digest_hex(&random))).expect("hex filename");
        // SAFETY: the owned directory fd and NUL-terminated name remain live.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                nonce.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returned a new fd, transferred exactly once.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| -> Result<()> {
            file.write_all(MAGIC)?;
            file.write_all(&(metadata.len() as u32).to_be_bytes())?;
            file.write_all(&(payload.0.len() as u64).to_be_bytes())?;
            file.write_all(&metadata)?;
            file.write_all(&payload.0)?;
            file.sync_all()?;
            // Atomic no-replace publication. Hardlink stays within the owned
            // directory and the private temporary name is removed below.
            // SAFETY: both relative names and the directory fd remain live.
            if unsafe {
                libc::linkat(
                    self.directory.as_raw_fd(),
                    nonce.as_ptr(),
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            Ok(())
        })();
        let unlinked = unsafe { libc::unlinkat(self.directory.as_raw_fd(), nonce.as_ptr(), 0) };
        if result.is_ok() {
            if unlinked != 0 {
                return Err(io::Error::last_os_error().into());
            }
            self.directory.sync_all()?;
        }
        result
    }

    /// Removes the artifact for one exact run, for example when its record is
    /// deleted. Absence is not an error; nothing else in the directory is touched.
    pub fn discard(&self, key: &CompletedRunKey) -> Result<()> {
        let name = key.name()?;
        // SAFETY: the owned directory fd and NUL-terminated name remain live.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Returns None only for an absent exact-run artifact. The expected key must
    /// come from current Engine run state; do not discover the newest filename or
    /// derive a key from an old checkpoint. A resumed run needs a different key.
    pub fn load(
        &self,
        record: &SessionRecord,
        expected: &CompletedRunKey,
    ) -> Result<Option<CompletedTerminal>> {
        let _admission = Admission::acquire()?;
        expected.check_record(record)?;
        let SessionStatus::Exited(exit) = &record.status else {
            return Err(StorageError::UnsupportedRecord);
        };
        check_exit(record, exit)?;
        let name = expected.name()?;
        // SAFETY: the owned directory fd and NUL-terminated name remain live.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        // SAFETY: successful openat returned a new fd, transferred exactly once.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let stat = file.metadata()?;
        if !stat.is_file()
            || stat.uid() != unsafe { libc::geteuid() }
            || stat.mode() & 0o077 != 0
            || stat.nlink() != 1
        {
            return Err(StorageError::Corrupt);
        }
        if stat.len() > (HEADER_BYTES + MAX_METADATA + MAX_CHECKPOINT_BYTES) as u64 {
            return Err(StorageError::TooLarge);
        }
        let mut header = [0u8; HEADER_BYTES];
        file.read_exact(&mut header)?;
        if &header[..8] != MAGIC {
            return Err(StorageError::Corrupt);
        }
        let metadata_len = u32::from_be_bytes(header[8..12].try_into().expect("header")) as usize;
        let payload_len = u64::from_be_bytes(header[12..20].try_into().expect("header"));
        if metadata_len > MAX_METADATA || payload_len > MAX_CHECKPOINT_BYTES as u64 {
            return Err(StorageError::TooLarge);
        }
        if stat.len() != HEADER_BYTES as u64 + metadata_len as u64 + payload_len {
            return Err(StorageError::Corrupt);
        }
        let mut metadata = vec![0u8; metadata_len];
        file.read_exact(&mut metadata)?;
        let metadata: Metadata =
            serde_json::from_slice(&metadata).map_err(|_| StorageError::Corrupt)?;
        if metadata.version != 1 || metadata.key != *expected || metadata.exit != *exit {
            return Err(StorageError::IdentityMismatch);
        }
        let mut payload = vec![0u8; payload_len as usize];
        file.read_exact(&mut payload)?;
        if <[u8; 32]>::from(Sha256::digest(&payload)) != metadata.checkpoint_sha256 {
            return Err(StorageError::Corrupt);
        }
        let mut rest = payload.as_slice();
        let grid_bytes = take_section(&mut rest)?;
        let history_bytes = take_section(&mut rest)?;
        let annotation_bytes = take_section(&mut rest)?;
        let keyboard_bytes = take_section(&mut rest)?;
        if !rest.is_empty()
            || annotation_bytes.len() > MAX_GRID_METADATA_BYTES
            || keyboard_bytes.len() > 8198
        {
            return Err(StorageError::Corrupt);
        }
        let history_rows = metadata.history_rows as usize;
        let cols = preflight_grid(grid_bytes, history_rows)?;
        preflight_history(history_bytes, history_rows, cols)?;
        let grid = GridUpdate::decode(grid_bytes).map_err(|_| StorageError::Corrupt)?;
        let history = GridRowCodec::decode_rows(history_bytes, history_rows)
            .map_err(|_| StorageError::Corrupt)?;
        let history_metadata: Vec<RowMetadata> =
            serde_json::from_slice(annotation_bytes).map_err(|_| StorageError::Corrupt)?;
        if !history_metadata.is_empty()
            && (history_metadata.len() != history_rows
                || history_metadata.iter().any(|row| !row.validate(cols)))
        {
            return Err(StorageError::Corrupt);
        }
        let keyboard_snapshot = if keyboard_bytes.is_empty() {
            None
        } else {
            Some(
                diri_terminal_state::KeyboardSnapshot::decode(keyboard_bytes)
                    .ok_or(StorageError::Corrupt)?,
            )
        };
        if metadata
            .keyboard
            .and_then(|keyboard| keyboard.enhancements)
            .map(|flags| flags.bits())
            != keyboard_snapshot
                .as_ref()
                .map(|snapshot| snapshot.current())
            || metadata.log_offset < expected.epoch_offset
        {
            return Err(StorageError::Corrupt);
        }
        let checkpoint = ScreenCheckpoint {
            grid,
            history,
            history_metadata,
            log_offset: metadata.log_offset,
            keyboard: metadata.keyboard,
            keyboard_snapshot,
            marker_buffer: Vec::new(),
            alt_screen: metadata.alt_screen,
            bracketed_paste: metadata.bracketed_paste,
            mouse: metadata.mouse,
        };
        Ok(Some(CompletedTerminal {
            checkpoint,
            exit: metadata.exit,
            coverage: CompletedCoverage::RetainedGridAndHistory,
        }))
    }
}

fn check_exit(record: &SessionRecord, exit: &ExitInfo) -> Result<()> {
    let valid = match exit.reason {
        ExitReason::Exited => exit.code.is_some() && exit.signal.is_none(),
        ExitReason::Signaled => exit.signal.is_some_and(|signal| signal > 0) && exit.code.is_none(),
        _ => false,
    };
    if !valid || !matches!(&record.status, SessionStatus::Exited(actual) if actual == exit) {
        return Err(StorageError::UnsupportedRecord);
    }
    Ok(())
}

fn check_capture_size(checkpoint: &ScreenCheckpoint) -> Result<()> {
    let cells = checkpoint
        .history
        .iter()
        .map(Vec::len)
        .chain(
            checkpoint
                .grid
                .changed_rows
                .iter()
                .map(|row| row.cells.len()),
        )
        .try_fold(0usize, |n, len| n.checked_add(len))
        .ok_or(StorageError::TooLarge)?;
    if cells > MAX_RETAINED_CELLS || checkpoint.history.len() > MAX_HISTORY_ROWS {
        return Err(StorageError::TooLarge);
    }
    if !checkpoint.history_metadata.is_empty()
        && (checkpoint.history_metadata.len() != checkpoint.history.len()
            || checkpoint
                .history_metadata
                .iter()
                .any(|row| !row.validate(checkpoint.grid.cols as usize)))
    {
        return Err(StorageError::Corrupt);
    }
    let mut metadata = LimitedBytes(Vec::new());
    serde_json::to_writer(&mut metadata, &checkpoint.history_metadata)
        .map_err(|_| StorageError::TooLarge)?;
    if metadata.0.len() > diri_proto::grid::MAX_GRID_METADATA_BYTES {
        return Err(StorageError::TooLarge);
    }
    let grid_metadata: Vec<_> = checkpoint
        .grid
        .changed_rows
        .iter()
        .filter(|row| !row.metadata.is_empty())
        .map(|row| (row.y, &row.metadata))
        .collect();
    metadata.0.clear();
    serde_json::to_writer(&mut metadata, &grid_metadata).map_err(|_| StorageError::TooLarge)?;
    if metadata.0.len() > diri_proto::grid::MAX_GRID_METADATA_BYTES {
        return Err(StorageError::TooLarge);
    }
    Ok(())
}

fn append_section(writer: &mut LimitedBytes, bytes: &[u8]) -> Result<()> {
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

fn take_section<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8]> {
    let header = bytes.get(..4).ok_or(StorageError::Corrupt)?;
    let length = u32::from_be_bytes(header.try_into().expect("section length")) as usize;
    let section = bytes.get(4..4 + length).ok_or(StorageError::Corrupt)?;
    *bytes = &bytes[4 + length..];
    Ok(section)
}

/// Validate expanded RLE size before the existing decoder allocates cells.
fn preflight_grid(grid: &[u8], history: usize) -> Result<usize> {
    if grid.len() < 11 {
        return Err(StorageError::Corrupt);
    }
    let cols = u16::from_be_bytes([grid[0], grid[1]]) as usize;
    let rows = u16::from_be_bytes([grid[2], grid[3]]) as usize;
    let count = u16::from_be_bytes([grid[9], grid[10]]) as usize;
    if cols == 0
        || rows == 0
        || history > MAX_HISTORY_ROWS
        || (rows + history) * cols > MAX_RETAINED_CELLS
    {
        return Err(StorageError::TooLarge);
    }
    if grid[8] & 2 == 0
        || grid[8] & !7 != 0
        || count != rows
        || u16::from_be_bytes([grid[4], grid[5]]) as usize >= cols
        || u16::from_be_bytes([grid[6], grid[7]]) as usize >= rows
    {
        return Err(StorageError::Corrupt);
    }
    let mut offset = 11;
    for row in 0..rows {
        let y = read_u16(grid, &mut offset)?;
        if y != row {
            return Err(StorageError::Corrupt);
        }
        preflight_row(grid, &mut offset, cols)?;
    }
    if grid[8] & 4 == 0 {
        if offset != grid.len() {
            return Err(StorageError::Corrupt);
        }
    } else {
        if grid.get(offset) != Some(&1) {
            return Err(StorageError::Corrupt);
        }
        let length = grid
            .get(offset + 1..offset + 5)
            .ok_or(StorageError::Corrupt)?;
        let length = u32::from_be_bytes(length.try_into().expect("metadata length")) as usize;
        if length > MAX_GRID_METADATA_BYTES || grid.len() != offset + 5 + length {
            return Err(StorageError::Corrupt);
        }
    }
    Ok(cols)
}

fn preflight_history(bytes: &[u8], rows: usize, cols: usize) -> Result<()> {
    let mut offset = 0;
    for _ in 0..rows {
        preflight_row(bytes, &mut offset, cols)?;
    }
    if offset != bytes.len() {
        return Err(StorageError::Corrupt);
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: &mut usize) -> Result<usize> {
    let slice = bytes
        .get(*offset..*offset + 2)
        .ok_or(StorageError::Corrupt)?;
    *offset += 2;
    Ok(u16::from_be_bytes([slice[0], slice[1]]) as usize)
}
fn preflight_row(bytes: &[u8], offset: &mut usize, cols: usize) -> Result<()> {
    let runs = read_u16(bytes, offset)?;
    let mut cells = 0;
    for _ in 0..runs {
        let repeat = read_u16(bytes, offset)?;
        if repeat == 0 || cells + repeat > cols || bytes.len().saturating_sub(*offset) < 14 {
            return Err(StorageError::Corrupt);
        }
        cells += repeat;
        *offset += 14;
    }
    if cells != cols {
        return Err(StorageError::Corrupt);
    }
    Ok(())
}
struct LimitedBytes(Vec<u8>);
impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_CHECKPOINT_BYTES {
            return Err(io::Error::other("completed checkpoint exceeds limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn is_artifact_name(name: &str) -> bool {
    name.strip_prefix("completed-")
        .and_then(|rest| rest.strip_suffix(".bin"))
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn digest_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut result, "{byte:02x}").expect("write String");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::process::{BootId, ProcessBirth, ProcessIdentity};
    use diri_proto::{AgentKind, ProjectId, Resumability, TitleSource};
    use std::os::unix::fs::PermissionsExt;

    /// The admission counter is process-wide, so store tests run one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(id: &str, exit: Option<ExitInfo>) -> SessionRecord {
        SessionRecord {
            attention_state: None,
            id: SessionId(id.into()),
            kind: AgentKind::SHELL,
            cwd: "/tmp".into(),
            project_id: ProjectId("p".into()),
            worktree_path: None,
            git_branch: None,
            title: "test".into(),
            title_source: TitleSource::Placeholder,
            account_profile: None,
            originating_prompt: None,
            agent_session_id: None,
            transcript_path: None,
            status: exit.map_or(SessionStatus::Working, SessionStatus::Exited),
            status_evidence: None,
            needs_input: None,
            resumability: Resumability::NotResumable,
            capabilities: None,
            parent: None,
            created_at: DateMillis(1_700_000_000_000.0),
            updated_at: DateMillis(1_700_000_000_000.0),
            last_turn_completed_at: None,
            last_seen_at: None,
            pinned: false,
            archived_at: None,
            host: None,
            remote_persistence: None,
            remote_connection: None,
            hibernation: None,
            memory_bytes: None,
            artifacts: None,
            pull_requests: None,
            listening_ports: None,
            foreground_agent: None,
        }
    }

    fn exited(code: i32) -> ExitInfo {
        ExitInfo {
            reason: ExitReason::Exited,
            code: Some(code),
            signal: None,
        }
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity::new(
            pid,
            ProcessBirth::Macos {
                boot_session: BootId::parse("0f0e0d0c-0b0a-0908-0706-050403020100").unwrap(),
                start_seconds: 1_700_000_000,
                start_microseconds: 42,
            },
        )
        .unwrap()
    }

    fn stat(
        pid: u32,
        alive: bool,
        child: Option<ProcessIdentity>,
        epoch_offset: Option<u64>,
    ) -> HolderStat {
        HolderStat {
            child_identity: child,
            child_pid: pid as i32,
            alive,
            log_offset: 900,
            foreground_pid: None,
            cols: Some(20),
            rows: Some(3),
            epoch_offset,
            secret_input: None,
        }
    }

    fn live_key(id: &str) -> CompletedRunKey {
        CompletedRunKey::capture(
            &record(id, None),
            &stat(4242, true, Some(identity(4242)), Some(100)),
        )
        .unwrap()
    }

    /// A real emulator capture: scrollback, a hyperlink annotation, an
    /// enhanced keyboard flag and bracketed paste, exactly as the pump would
    /// checkpoint it after the final drain.
    fn checkpoint(log_offset: u64) -> ScreenCheckpoint {
        let mut screen = diri_terminal_state::HeadlessScreen::new_with_keyboard_enhancements(20, 3);
        screen.feed(b"first\r\nsecond\r\nthird\r\n\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ fourth\r\nfifth\r\nsixth\r\n");
        screen.feed(b"\x1b[>1u\x1b[?2004h");
        let grid = screen.grid_update(true);
        let history = screen.history_snapshot();
        let history_metadata = screen.history_metadata();
        assert!(!history.is_empty(), "the fixture must exercise history");
        ScreenCheckpoint {
            keyboard_snapshot: screen.keyboard_snapshot(),
            keyboard: Some(screen.keyboard_state()),
            log_offset,
            history,
            history_metadata,
            grid,
            marker_buffer: Vec::new(),
            alt_screen: screen.is_alt_screen(),
            bracketed_paste: screen.bracketed_paste(),
            mouse: screen.mouse_modes(),
        }
    }

    fn private_dir(root: &Path) -> std::path::PathBuf {
        let dir = root.join("completed");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn only_artifact(dir: &Path) -> std::path::PathBuf {
        let entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one published artifact, no nonce left behind"
        );
        let path = entries[0].path();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("completed-") && name.ends_with(".bin"),
            "{name}"
        );
        path
    }

    fn published() -> (
        tempfile::TempDir,
        CompletedTerminalStore,
        CompletedRunKey,
        ScreenCheckpoint,
        SessionRecord,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let store = CompletedTerminalStore::open(&private_dir(temp.path())).unwrap();
        let key = live_key("s1");
        let checkpoint = checkpoint(900);
        let done = record("s1", Some(exited(0)));
        store.publish(&done, &key, &checkpoint, &exited(0)).unwrap();
        (temp, store, key, checkpoint, done)
    }

    /// Re-frames an artifact from its parts so tests can produce hostile but
    /// integrity-consistent files. Metadata is JSON; the payload hash inside it
    /// is recomputed from the supplied payload.
    fn write_artifact(
        dir: &Path,
        key: &CompletedRunKey,
        mut metadata: serde_json::Value,
        payload: &[u8],
    ) {
        metadata["checkpointSha256"] =
            serde_json::to_value(<[u8; 32]>::from(Sha256::digest(payload))).unwrap();
        let metadata = serde_json::to_vec(&metadata).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&metadata);
        bytes.extend_from_slice(payload);
        let path = dir.join(key.name().unwrap().to_str().unwrap());
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn read_artifact(path: &Path) -> (serde_json::Value, Vec<u8>) {
        let bytes = std::fs::read(path).unwrap();
        let metadata_len = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let metadata =
            serde_json::from_slice(&bytes[HEADER_BYTES..HEADER_BYTES + metadata_len]).unwrap();
        (metadata, bytes[HEADER_BYTES + metadata_len..].to_vec())
    }

    #[test]
    fn publish_then_load_round_trips_the_exact_run() {
        let _serial = serial();
        let (temp, store, key, checkpoint, done) = published();
        let path = only_artifact(&temp.path().join("completed"));
        let stat = std::fs::metadata(&path).unwrap();
        assert_eq!(stat.permissions().mode() & 0o777, 0o600);

        let loaded = store
            .load(&done, &key)
            .unwrap()
            .expect("the exact run exists");
        assert_eq!(loaded.coverage, CompletedCoverage::RetainedGridAndHistory);
        assert_eq!(loaded.exit, exited(0));
        let restored = loaded.checkpoint;
        assert_eq!(restored.grid, checkpoint.grid);
        assert_eq!(restored.history, checkpoint.history);
        assert_eq!(restored.history_metadata, checkpoint.history_metadata);
        assert!(
            restored
                .history_metadata
                .iter()
                .any(|row| !row.links.is_empty()),
            "the hyperlink annotation survived"
        );
        assert_eq!(restored.keyboard, checkpoint.keyboard);
        assert_eq!(
            restored.keyboard_snapshot.as_ref().map(|s| s.encode()),
            checkpoint.keyboard_snapshot.as_ref().map(|s| s.encode())
        );
        assert_eq!(restored.log_offset, 900);
        assert!(restored.marker_buffer.is_empty());
        assert_eq!(restored.bracketed_paste, checkpoint.bracketed_paste);
        assert!(restored.bracketed_paste);
        assert_eq!(restored.alt_screen, checkpoint.alt_screen);
        assert_eq!(restored.mouse, checkpoint.mouse);
    }

    #[test]
    fn capture_requires_a_local_record_and_a_live_verified_holder() {
        let _serial = serial();
        let live = record("s1", None);
        let ok = stat(4242, true, Some(identity(4242)), Some(100));
        assert!(CompletedRunKey::capture(&live, &ok).is_ok());

        let mut remote = live.clone();
        remote.host = Some("forge".into());
        assert!(matches!(
            CompletedRunKey::capture(&remote, &ok),
            Err(StorageError::UnsupportedRecord)
        ));
        assert!(
            matches!(
                CompletedRunKey::capture(
                    &live,
                    &stat(4242, false, Some(identity(4242)), Some(100))
                ),
                Err(StorageError::UnsupportedRecord)
            ),
            "a dead Holder cannot vouch for its child any more"
        );
        assert!(
            matches!(
                CompletedRunKey::capture(&live, &stat(4242, true, None, Some(100))),
                Err(StorageError::IdentityMismatch)
            ),
            "an old Holder without birth identity is not bindable"
        );
        assert!(
            matches!(
                CompletedRunKey::capture(&live, &stat(4243, true, Some(identity(4242)), Some(100))),
                Err(StorageError::IdentityMismatch)
            ),
            "identity and childPID must agree"
        );
        assert!(matches!(
            CompletedRunKey::capture(&live, &stat(4242, true, Some(identity(4242)), None)),
            Err(StorageError::IdentityMismatch)
        ));
        assert!(
            matches!(
                CompletedRunKey::capture(&live, &stat(4242, true, Some(identity(4242)), Some(901))),
                Err(StorageError::IdentityMismatch)
            ),
            "an epoch beyond the log is not a real incarnation"
        );
    }

    #[test]
    fn a_different_run_of_the_same_session_is_absent_not_substituted() {
        let _serial = serial();
        let (_temp, store, _key, _checkpoint, done) = published();
        // Same session id, same record, but the Holder relaunched the child:
        // a new birth or epoch is a different run and must not find the old
        // artifact by name.
        let relaunched = CompletedRunKey::capture(
            &record("s1", None),
            &stat(4242, true, Some(identity(4242)), Some(400)),
        )
        .unwrap();
        assert!(store.load(&done, &relaunched).unwrap().is_none());
        let other_child = CompletedRunKey::capture(
            &record("s1", None),
            &stat(5151, true, Some(identity(5151)), Some(100)),
        )
        .unwrap();
        assert!(store.load(&done, &other_child).unwrap().is_none());
    }

    #[test]
    fn a_key_from_another_record_is_an_identity_mismatch() {
        let _serial = serial();
        let (_temp, store, key, checkpoint, _done) = published();
        let other = record("s2", Some(exited(0)));
        assert!(matches!(
            store.load(&other, &key),
            Err(StorageError::IdentityMismatch)
        ));
        let mut reused_id = record("s1", Some(exited(0)));
        reused_id.created_at = DateMillis(1_700_000_000_001.0);
        assert!(matches!(
            store.load(&reused_id, &key),
            Err(StorageError::IdentityMismatch)
        ));
        assert!(matches!(
            store.publish(&other, &key, &checkpoint, &exited(0)),
            Err(StorageError::IdentityMismatch)
        ));
        let mut remote = record("s1", Some(exited(0)));
        remote.host = Some("forge".into());
        assert!(matches!(
            store.load(&remote, &key),
            Err(StorageError::UnsupportedRecord)
        ));
    }

    #[test]
    fn publish_never_replaces_an_existing_run_artifact() {
        let _serial = serial();
        let (temp, store, key, _checkpoint, done) = published();
        let dir = temp.path().join("completed");
        let before = std::fs::read(only_artifact(&dir)).unwrap();
        let mut other = checkpoint(900);
        other.history.clear();
        other.history_metadata.clear();
        let error = store.publish(&done, &key, &other, &exited(0)).unwrap_err();
        assert!(
            matches!(&error, StorageError::Io(io) if io.kind() == io::ErrorKind::AlreadyExists),
            "{error}"
        );
        assert_eq!(
            std::fs::read(only_artifact(&dir)).unwrap(),
            before,
            "the first artifact is immutable"
        );
    }

    #[test]
    fn exit_facts_must_be_genuine_and_match_the_record() {
        let _serial = serial();
        let temp = tempfile::tempdir().unwrap();
        let store = CompletedTerminalStore::open(&private_dir(temp.path())).unwrap();
        let key = live_key("s1");
        let checkpoint = checkpoint(900);
        let restart = ExitInfo {
            reason: ExitReason::DaemonRestart,
            code: None,
            signal: None,
        };
        assert!(
            matches!(
                store.publish(
                    &record("s1", Some(restart.clone())),
                    &key,
                    &checkpoint,
                    &restart
                ),
                Err(StorageError::UnsupportedRecord)
            ),
            "a synthesized exit is not an observed one"
        );
        let mixed = ExitInfo {
            reason: ExitReason::Exited,
            code: Some(0),
            signal: Some(9),
        };
        assert!(matches!(
            store.publish(
                &record("s1", Some(mixed.clone())),
                &key,
                &checkpoint,
                &mixed
            ),
            Err(StorageError::UnsupportedRecord)
        ));
        assert!(
            matches!(
                store.publish(
                    &record("s1", Some(exited(0))),
                    &key,
                    &checkpoint,
                    &exited(1)
                ),
                Err(StorageError::UnsupportedRecord)
            ),
            "the supplied exit must be the record's exit"
        );
        assert!(
            matches!(
                store.publish(&record("s1", None), &key, &checkpoint, &exited(0)),
                Err(StorageError::UnsupportedRecord)
            ),
            "a live record has nothing completed to publish"
        );

        let signalled = ExitInfo {
            reason: ExitReason::Signaled,
            code: None,
            signal: Some(15),
        };
        store
            .publish(
                &record("s1", Some(signalled.clone())),
                &key,
                &checkpoint,
                &signalled,
            )
            .unwrap();
        assert!(
            matches!(
                store.load(&record("s1", Some(exited(0))), &key),
                Err(StorageError::IdentityMismatch)
            ),
            "an artifact for another exit is not this run's"
        );
        assert!(matches!(
            store.load(&record("s1", None), &key),
            Err(StorageError::UnsupportedRecord)
        ));
        assert_eq!(
            store
                .load(&record("s1", Some(signalled.clone())), &key)
                .unwrap()
                .unwrap()
                .exit,
            signalled
        );
    }

    #[test]
    fn publish_refuses_partial_markers_epoch_regressions_and_oversized_captures() {
        let _serial = serial();
        let temp = tempfile::tempdir().unwrap();
        let store = CompletedTerminalStore::open(&private_dir(temp.path())).unwrap();
        let key = live_key("s1");
        let done = record("s1", Some(exited(0)));
        let mut partial = checkpoint(900);
        partial.marker_buffer = vec![0x1b, b']'];
        assert!(matches!(
            store.publish(&done, &key, &partial, &exited(0)),
            Err(StorageError::Corrupt)
        ));
        let before_epoch = checkpoint(50);
        assert!(
            matches!(
                store.publish(&done, &key, &before_epoch, &exited(0)),
                Err(StorageError::Corrupt)
            ),
            "a capture older than this incarnation's first byte is not this run"
        );
        let mut deep = checkpoint(900);
        deep.history = vec![vec![diri_proto::grid::GridCell::BLANK; 20]; MAX_HISTORY_ROWS + 1];
        deep.history_metadata.clear();
        assert!(matches!(
            store.publish(&done, &key, &deep, &exited(0)),
            Err(StorageError::TooLarge)
        ));
        let mut wide = checkpoint(900);
        wide.history = vec![vec![diri_proto::grid::GridCell::BLANK; 60_000]; 20];
        wide.history_metadata.clear();
        assert!(matches!(
            store.publish(&done, &key, &wide, &exited(0)),
            Err(StorageError::TooLarge)
        ));
        let mut mismatched = checkpoint(900);
        mismatched.history_metadata.push(RowMetadata::default());
        assert!(matches!(
            store.publish(&done, &key, &mismatched, &exited(0)),
            Err(StorageError::Corrupt)
        ));
        let mut inconsistent_keyboard = checkpoint(900);
        inconsistent_keyboard.keyboard_snapshot = None;
        assert!(
            matches!(
                store.publish(&done, &key, &inconsistent_keyboard, &exited(0)),
                Err(StorageError::Corrupt)
            ),
            "known enhanced flags without their snapshot cannot be restored"
        );
        assert!(
            std::fs::read_dir(temp.path().join("completed"))
                .unwrap()
                .next()
                .is_none(),
            "no artifact or nonce remains"
        );
    }

    #[test]
    fn load_rejects_tampered_truncated_and_oversized_artifacts() {
        let _serial = serial();
        let (temp, store, key, _checkpoint, done) = published();
        let dir = temp.path().join("completed");
        let path = only_artifact(&dir);
        let original = std::fs::read(&path).unwrap();
        let (metadata, payload) = read_artifact(&path);

        let mut flipped = payload.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        // Integrity is checked before any section decoding.
        let mut bytes = original.clone();
        bytes.truncate(original.len() - payload.len());
        bytes.extend_from_slice(&flipped);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            store.load(&done, &key),
            Err(StorageError::Corrupt)
        ));

        std::fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(
            matches!(store.load(&done, &key), Err(StorageError::Corrupt)),
            "length must match the header"
        );

        let mut bad_magic = original.clone();
        bad_magic[0] ^= 1;
        std::fs::write(&path, &bad_magic).unwrap();
        assert!(matches!(
            store.load(&done, &key),
            Err(StorageError::Corrupt)
        ));

        let mut huge = original.clone();
        huge[12..20].copy_from_slice(&((MAX_CHECKPOINT_BYTES as u64) + 1).to_be_bytes());
        std::fs::write(&path, &huge).unwrap();
        assert!(
            matches!(store.load(&done, &key), Err(StorageError::TooLarge)),
            "declared sizes are gated before allocation"
        );

        // Metadata that is consistent with the payload hash but not this run.
        let mut wrong_version = metadata.clone();
        wrong_version["version"] = serde_json::json!(2);
        write_artifact(&dir, &key, wrong_version, &payload);
        assert!(matches!(
            store.load(&done, &key),
            Err(StorageError::IdentityMismatch)
        ));
        let mut wrong_offset = metadata.clone();
        wrong_offset["logOffset"] = serde_json::json!(10);
        write_artifact(&dir, &key, wrong_offset, &payload);
        assert!(
            matches!(store.load(&done, &key), Err(StorageError::Corrupt)),
            "an offset below the epoch is not this run"
        );

        write_artifact(&dir, &key, metadata, &payload);
        assert!(
            store.load(&done, &key).unwrap().is_some(),
            "an untouched re-framing loads again"
        );
    }

    #[test]
    fn expansion_bombs_are_rejected_before_cells_are_allocated() {
        let _serial = serial();
        let (temp, store, key, checkpoint, done) = published();
        let dir = temp.path().join("completed");
        let (metadata, _) = read_artifact(&only_artifact(&dir));
        let cols = checkpoint.grid.cols as usize;

        // History row whose single run claims more cells than the grid width.
        let mut history = Vec::new();
        history.extend_from_slice(&1_u16.to_be_bytes());
        history.extend_from_slice(&((cols as u16) + 1).to_be_bytes());
        history.extend_from_slice(&[0; 14]);
        let grid = checkpoint.grid.encode().unwrap();
        let mut payload = Vec::new();
        for section in [grid.as_slice(), history.as_slice(), b"[]", b""] {
            payload.extend_from_slice(&(section.len() as u32).to_be_bytes());
            payload.extend_from_slice(section);
        }
        let mut bomb = metadata.clone();
        bomb["historyRows"] = serde_json::json!(1);
        write_artifact(&dir, &key, bomb, &payload);
        assert!(matches!(
            store.load(&done, &key),
            Err(StorageError::Corrupt)
        ));

        // A grid header that declares more cells than the retained budget.
        let mut grid = checkpoint.grid.encode().unwrap();
        grid[2..4].copy_from_slice(&u16::MAX.to_be_bytes());
        grid[9..11].copy_from_slice(&u16::MAX.to_be_bytes());
        let history = GridRowCodec::encode_rows(&checkpoint.history).unwrap();
        let annotations = serde_json::to_vec(&checkpoint.history_metadata).unwrap();
        let keyboard = checkpoint.keyboard_snapshot.as_ref().unwrap().encode();
        let mut payload = Vec::new();
        for section in [
            grid.as_slice(),
            history.as_slice(),
            annotations.as_slice(),
            keyboard.as_slice(),
        ] {
            payload.extend_from_slice(&(section.len() as u32).to_be_bytes());
            payload.extend_from_slice(section);
        }
        write_artifact(&dir, &key, metadata, &payload);
        assert!(matches!(
            store.load(&done, &key),
            Err(StorageError::TooLarge)
        ));
    }

    #[test]
    fn special_files_and_shared_directories_are_refused() {
        let _serial = serial();
        let (temp, store, key, _checkpoint, done) = published();
        let dir = temp.path().join("completed");
        let path = only_artifact(&dir);
        let real = temp.path().join("elsewhere.bin");
        std::fs::rename(&path, &real).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(
            matches!(store.load(&done, &key), Err(StorageError::Io(_))),
            "a symlinked artifact is never followed"
        );
        std::fs::remove_file(&path).unwrap();

        assert_eq!(
            unsafe {
                libc::mkfifo(
                    CString::new(path.to_str().unwrap()).unwrap().as_ptr(),
                    0o600,
                )
            },
            0
        );
        assert!(
            matches!(store.load(&done, &key), Err(StorageError::Corrupt)),
            "a FIFO must not block the loader"
        );
        std::fs::remove_file(&path).unwrap();

        std::fs::rename(&real, &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(store.load(&done, &key), Err(StorageError::Corrupt)),
            "group/world readable artifacts are not ours"
        );

        let shared = temp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            CompletedTerminalStore::open(&shared),
            Err(StorageError::Corrupt)
        ));
        let link = temp.path().join("linked");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(matches!(
            CompletedTerminalStore::open(&link),
            Err(StorageError::Io(_))
        ));
        assert!(
            matches!(
                CompletedTerminalStore::open(&temp.path().join("missing")),
                Err(StorageError::Io(_))
            ),
            "the store never creates its directory"
        );
    }

    #[test]
    fn bind_reuses_the_captured_run_and_discard_removes_only_that_artifact() {
        let _serial = serial();
        let (temp, store, key, _checkpoint, done) = published();
        let bound = CompletedRunKey::bind(&record("s1", None), identity(4242), 100).unwrap();
        assert_eq!(
            bound, key,
            "a retained verified run binds to the same artifact"
        );
        assert!(bound.is_run(identity(4242), 100));
        assert!(!bound.is_run(identity(4242), 101));
        let mut remote = record("s1", None);
        remote.host = Some("forge".into());
        assert!(matches!(
            CompletedRunKey::bind(&remote, identity(4242), 100),
            Err(StorageError::UnsupportedRecord)
        ));
        let other = CompletedRunKey::bind(&record("s1", None), identity(4242), 400).unwrap();
        store.discard(&other).unwrap();
        assert!(
            store.load(&done, &key).unwrap().is_some(),
            "another run's discard is a no-op"
        );
        store.discard(&key).unwrap();
        assert!(store.load(&done, &key).unwrap().is_none());
        store.discard(&key).unwrap();
        assert!(
            std::fs::read_dir(temp.path().join("completed"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn retention_removes_orphans_first_then_the_oldest_bound_artifacts() {
        let _serial = serial();
        let temp = tempfile::tempdir().unwrap();
        let dir = private_dir(temp.path());
        let store = CompletedTerminalStore::open(&dir).unwrap();
        let mut names = Vec::new();
        for (index, id) in ["s1", "s2", "s3", "orphan"].iter().enumerate() {
            let key = live_key(id);
            let done = record(id, Some(exited(0)));
            store
                .publish(&done, &key, &checkpoint(900), &exited(0))
                .unwrap();
            let name = key.artifact_name().unwrap();
            // Distinct ages, oldest first, independent of filesystem timing.
            let file = std::fs::File::options()
                .write(true)
                .open(dir.join(&name))
                .unwrap();
            file.set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + index as u64),
            )
            .unwrap();
            names.push(name);
        }
        std::fs::write(dir.join(".completed-stale.tmp"), b"nonce").unwrap();
        std::fs::write(dir.join("unrelated.txt"), b"not ours").unwrap();
        let keep: std::collections::HashSet<String> = names[..3].iter().cloned().collect();

        let report = store.retain(&keep).unwrap();
        assert_eq!(report.removed_orphans, 1, "{report:?}");
        assert_eq!(report.evicted, 0);
        assert_eq!(report.retained, 3);
        assert!(!dir.join(&names[3]).exists());
        assert!(
            dir.join("unrelated.txt").exists(),
            "foreign files are never touched"
        );
        assert!(
            dir.join(".completed-stale.tmp").exists(),
            "nonces belong to writers"
        );

        let report = store.retain_within(&keep, 2, u64::MAX).unwrap();
        assert_eq!(report.evicted, 1);
        assert!(
            !dir.join(&names[0]).exists(),
            "the oldest bound artifact goes first"
        );
        assert!(dir.join(&names[1]).exists() && dir.join(&names[2]).exists());

        let size = std::fs::metadata(dir.join(&names[2])).unwrap().len();
        let report = store.retain_within(&keep, 10, size).unwrap();
        assert_eq!(report.evicted, 1, "{report:?}");
        assert_eq!(report.retained, 1);
        assert_eq!(report.retained_bytes, size);
        assert!(
            dir.join(&names[2]).exists(),
            "the newest survives a byte bound"
        );
        assert!(
            store
                .load(&record("s3", Some(exited(0))), &live_key("s3"))
                .unwrap()
                .is_some(),
            "a retained artifact still loads exactly"
        );
    }

    #[test]
    fn at_most_two_storage_operations_run_concurrently() {
        let _serial = serial();
        let (_temp, store, key, _checkpoint, done) = published();
        let first = Admission::acquire().unwrap();
        let second = Admission::acquire().unwrap();
        assert!(matches!(store.load(&done, &key), Err(StorageError::Busy)));
        drop(first);
        assert!(store.load(&done, &key).unwrap().is_some());
        drop(second);
    }
}
