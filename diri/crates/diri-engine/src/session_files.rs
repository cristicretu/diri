//! Every file the Engine keeps per session, and the one place that deletes
//! them.
//!
//! A session leaves files in two places:
//!
//! * the logs directory — `<id>.bin` (the output ring, written by the Engine
//!   or the local Holder), `<id>.screen.plist` (the screen checkpoint, plus
//!   its `.plist.tmp` while a write is in flight) and `<id>.attention.sqlite`
//!   (the status reducer's attention store, plus SQLite's rollback-journal /
//!   WAL companions);
//! * the recovery root — `sessions/<id>/`, whose Engine-owned files are
//!   removed by [`diri_proto::recovery::SessionRecoveryStore::remove_owned_files`].
//!
//! Removing a session deletes all of them through [`remove_log_files`] so a
//! new sidecar only has to be added to [`LOG_SUFFIXES`]. [`sweep_orphans`] is
//! the one-shot startup backstop for files earlier builds (or a crash between
//! persist and unlink) left behind. It is conservative by construction: it
//! only considers regular files whose whole name is `s_<12 lowercase hex>`
//! plus one of those suffixes, never follows symlinks, keeps anything
//! modified recently, and keeps every id the caller says is referenced.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Every per-session file in the logs directory, as a suffix after the id.
/// The SQLite companions exist only while a connection is (or was, at a
/// crash) mid-transaction; deleting them with the database is correct.
pub const LOG_SUFFIXES: &[&str] = &[
    ".bin",
    ".screen.plist",
    ".screen.plist.tmp",
    ".attention.sqlite",
    ".attention.sqlite-journal",
    ".attention.sqlite-wal",
    ".attention.sqlite-shm",
];

/// Deletes every per-session file in `logs_dir` for `id`. Missing files are
/// fine; the first other error is returned after every path was attempted.
pub fn remove_log_files(logs_dir: &Path, id: &str) -> io::Result<()> {
    let mut first_error = None;
    for suffix in LOG_SUFFIXES {
        match std::fs::remove_file(logs_dir.join(format!("{id}{suffix}"))) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// The session id a daemon-created file name belongs to: exactly `s_` plus
/// twelve lowercase hex digits (see `control::next_session_id`), followed by
/// one of [`LOG_SUFFIXES`]. Anything else is not ours to judge.
pub fn log_file_session_id(name: &str) -> Option<&str> {
    let (id, suffix) = (name.get(..14)?, name.get(14..)?);
    (is_daemon_session_id(id) && LOG_SUFFIXES.contains(&suffix)).then_some(id)
}

/// `s_` plus exactly twelve lowercase hex digits.
pub fn is_daemon_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 14
        && bytes.starts_with(b"s_")
        && bytes[2..]
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Clone, Debug)]
pub struct SweepOptions {
    /// Files modified more recently than this are kept: a spawn in progress
    /// creates its log before its record is necessarily visible.
    pub min_age: Duration,
    /// Stop after examining this many directory entries (one-shot bound).
    pub max_entries: usize,
    /// Count what would be removed without removing anything.
    pub dry_run: bool,
}

impl Default for SweepOptions {
    fn default() -> Self {
        Self {
            min_age: Duration::from_secs(15 * 60),
            max_entries: 200_000,
            dry_run: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Orphan log-directory files removed (or that would be, on a dry run).
    pub removed_files: usize,
    pub removed_bytes: u64,
    /// Orphan recovery directories whose Engine-owned files were removed.
    pub removed_recovery_dirs: usize,
    /// Files of referenced sessions.
    pub kept_referenced: usize,
    /// Orphan-looking files kept because they changed within `min_age`.
    pub kept_recent: usize,
    /// Entries that are not a daemon session file (other names, symlinks,
    /// directories, other file types).
    pub skipped_foreign: usize,
    /// Removals that failed (permissions, races); they are left for next time.
    pub failed: usize,
    /// The entry budget ran out before the directory was fully examined.
    pub truncated: bool,
    pub elapsed: Duration,
}

/// Removes per-session files whose id is not in `referenced`.
///
/// `recovery_root` (`sessions/`) is swept the same way, but only the
/// Engine-owned files inside an orphan's directory are removed; provider
/// storage beside them, and the directory itself unless empty, stay.
pub fn sweep_orphans(
    logs_dir: &Path,
    recovery_root: Option<&Path>,
    referenced: &HashSet<String>,
    options: &SweepOptions,
) -> SweepReport {
    let started = Instant::now();
    let now = SystemTime::now();
    let mut report = SweepReport::default();
    let mut budget = options.max_entries;
    let is_recent = |metadata: &std::fs::Metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            // A timestamp in the future (clock skew) counts as recent.
            .is_none_or(|age| age < options.min_age)
    };

    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries {
            if budget == 0 {
                report.truncated = true;
                break;
            }
            budget -= 1;
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(log_file_session_id) else {
                report.skipped_foreign += 1;
                continue;
            };
            // `DirEntry::metadata` does not traverse symlinks.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.file_type().is_file() {
                report.skipped_foreign += 1;
                continue;
            }
            if referenced.contains(id) {
                report.kept_referenced += 1;
                continue;
            }
            if is_recent(&metadata) {
                report.kept_recent += 1;
                continue;
            }
            if options.dry_run {
                report.removed_files += 1;
                report.removed_bytes += metadata.len();
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    report.removed_files += 1;
                    report.removed_bytes += metadata.len();
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => report.failed += 1,
            }
        }
    }

    if let Some(root) = recovery_root
        && let Ok(entries) = std::fs::read_dir(root)
    {
        for entry in entries {
            if budget == 0 {
                report.truncated = true;
                break;
            }
            budget -= 1;
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(id) = name.to_str().filter(|name| is_daemon_session_id(name)) else {
                report.skipped_foreign += 1;
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.file_type().is_dir() {
                report.skipped_foreign += 1;
                continue;
            }
            if referenced.contains(id) {
                report.kept_referenced += 1;
                continue;
            }
            // A capsule write lands in this directory before a spawn's record
            // is persisted; its mtime moves with every file created in it.
            if is_recent(&metadata) {
                report.kept_recent += 1;
                continue;
            }
            if options.dry_run {
                report.removed_recovery_dirs += 1;
                continue;
            }
            match diri_proto::recovery::SessionRecoveryStore::new(entry.path()).remove_owned_files()
            {
                Ok(()) => report.removed_recovery_dirs += 1,
                Err(_) => report.failed += 1,
            }
        }
    }

    report.elapsed = started.elapsed();
    report
}

/// Ids named by `<id><suffix>` entries in `directory` (holder sockets and pid
/// files, remote bindings). Names are enough: a live holder or a binding
/// whose file no longer parses still owns its session's files.
pub fn ids_named_in(directory: &Path, suffixes: &[&str]) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = suffixes
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix))?;
            is_daemon_session_id(id).then(|| id.to_owned())
        })
        .collect()
}

/// The daemon's one-shot startup sweep. `state_loaded` must be false when the
/// state file failed to load: an empty Registry then says nothing about which
/// files are orphans, so nothing is swept. The same holds for a Registry with
/// no records at all (a wiped or brand-new state).
pub fn startup_sweep(
    registry: &std::sync::Mutex<crate::registry::Registry>,
    state_loaded: bool,
    logs_dir: &Path,
    holders_dir: &Path,
    remote_bindings_dir: &Path,
    options: &SweepOptions,
) -> Option<SweepReport> {
    if !state_loaded {
        return None;
    }
    let (mut referenced, recovery_root) = {
        let registry = registry.lock().ok()?;
        if registry.record_count() == 0 {
            return None;
        }
        (
            registry.referenced_session_ids(),
            registry.recovery_root().to_path_buf(),
        )
    };
    let holders = crate::holder::paths::HolderPaths::new(holders_dir, "probe").directory;
    referenced.extend(ids_named_in(&holders, &[".sock", ".pid"]));
    referenced.extend(ids_named_in(remote_bindings_dir, &[".json"]));
    Some(sweep_orphans(
        logs_dir,
        Some(&recovery_root),
        &referenced,
        options,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_daemon_names_parse() {
        assert_eq!(
            log_file_session_id("s_0123456789ab.bin"),
            Some("s_0123456789ab")
        );
        assert_eq!(
            log_file_session_id("s_0123456789ab.attention.sqlite-wal"),
            Some("s_0123456789ab")
        );
        for foreign in [
            "s_0123456789ab.txt",
            "s_0123456789AB.bin",
            "s_0123456789a.bin",
            "s_0123456789abc.bin",
            "x_0123456789ab.bin",
            "s_0123456789ab.bin.bak",
            "s_0123456789ab",
            "dirijord-rs.boot.log",
            "completed-00ff.bin",
            "s_ü123456789a.bin",
        ] {
            assert_eq!(log_file_session_id(foreign), None, "{foreign}");
        }
    }
}
