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

/// The suffix of a Holder's child record (see
/// [`crate::holder::paths::HolderPaths::child_record`]). The Holder writes it
/// at spawn and, unlike its socket and pid file, never removes it, so an
/// Engine that arrives after the child exited can still bind the run.
pub const HOLDER_RECORD_SUFFIX: &str = ".child";

/// Deletes the Holder child record of a session that is gone for good. Once
/// its record is removed nothing can bind that run, and the Holder leaves the
/// file behind: one per session ever started, forever.
pub fn remove_holder_record(holders_dir: &Path, id: &str) -> io::Result<()> {
    let path = crate::holder::paths::HolderPaths::new(holders_dir, id).child_record();
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Removes child records in the Holders directory (`holders` is the resolved
/// one, see [`crate::holder::paths::safe_directory`]) whose session is not in
/// `referenced`. Same rules as [`sweep_orphans`]: exact daemon names only,
/// regular files only, recent files kept. Counts go into `report`.
pub fn sweep_holder_records(
    holders: &Path,
    referenced: &HashSet<String>,
    options: &SweepOptions,
    report: &mut SweepReport,
) {
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(holders) else {
        return;
    };
    let mut budget = options.max_entries;
    for entry in entries {
        if budget == 0 {
            report.truncated = true;
            break;
        }
        budget -= 1;
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(HOLDER_RECORD_SUFFIX))
            .filter(|id| is_daemon_session_id(id))
        else {
            continue;
        };
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
        let recent = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age < options.min_age);
        if recent {
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
    let mut report = sweep_orphans(logs_dir, Some(&recovery_root), &referenced, options);
    let started = Instant::now();
    sweep_holder_records(&holders, &referenced, options, &mut report);
    report.elapsed += started.elapsed();
    Some(report)
}

/// Upper bound on entries one [`tighten_private_roots`] pass inspects. A real
/// install holds a few hundred; the bound only stops a pathological tree.
pub const TIGHTEN_MAX_ENTRIES: usize = 50_000;

/// Every directory Diri writes its own state into for `home`: data, state,
/// config, runtime and cache, plus the app's preference directory. Nested
/// and duplicate roots collapse into their outermost one. A root that is
/// `home` itself or one of its ancestors (a misconfigured override) is never
/// returned: those hold the user's files, not Diri's.
pub fn private_roots(home: &Path) -> Vec<std::path::PathBuf> {
    use diri_proto::paths::DirijorPaths;
    let mut candidates = vec![
        DirijorPaths::app_support(home),
        DirijorPaths::state_dir(home),
        DirijorPaths::config_dir(home),
        DirijorPaths::runtime_dir(home),
        DirijorPaths::cache_dir(home),
    ];
    if let Some(prefs) = DirijorPaths::prefs_file(home).parent() {
        candidates.push(prefs.to_path_buf());
    }
    outermost_private_roots(home, candidates)
}

fn outermost_private_roots(
    home: &Path,
    mut candidates: Vec<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    candidates.retain(|root| root.is_absolute() && !home.starts_with(root));
    candidates.sort();
    candidates.dedup();
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for root in candidates {
        if !roots.iter().any(|outer| root.starts_with(outer)) {
            roots.push(root);
        }
    }
    roots
}

/// One-shot startup repair for state written before every writer created its
/// files owner-only: strips group/other bits under [`private_roots`]. Runs off
/// the accept path, once per Engine start; it is never repeated while idle.
pub fn tighten_private_roots(home: &Path) -> diri_platform::private::Tightened {
    tighten_roots(&private_roots(home))
}

fn tighten_roots(roots: &[std::path::PathBuf]) -> diri_platform::private::Tightened {
    let mut total = diri_platform::private::Tightened::default();
    for root in roots {
        let remaining = TIGHTEN_MAX_ENTRIES.saturating_sub(total.visited);
        let pass = diri_platform::private::tighten_tree(root, remaining);
        total.visited += pass.visited;
        total.changed += pass.changed;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_records_of_gone_sessions_are_swept() {
        let temp = tempfile::tempdir().expect("temp");
        // Where the Holders of a `temp` holders directory really live (a
        // long temp path resolves to a short one under /tmp).
        let resolved = crate::holder::paths::HolderPaths::new(temp.path(), "probe").directory;
        std::fs::create_dir_all(&resolved).expect("holders dir");
        let dir = resolved.as_path();
        let old = SystemTime::now() - Duration::from_secs(3600);
        let write = |name: &str, aged: bool| {
            let path = dir.join(name);
            std::fs::write(&path, b"{}").expect("write");
            if aged {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .and_then(|file| file.set_modified(old))
                    .expect("age");
            }
        };
        write("s_00000000000a.child", true); // removed long ago
        write("s_00000000000b.child", true); // still a record
        write("s_00000000000c.child", false); // just spawned
        write("s_00000000000d.child.bak", true); // not ours
        write("manager-v2.pid", true);
        let referenced = HashSet::from(["s_00000000000b".to_owned()]);
        let mut report = SweepReport::default();
        sweep_holder_records(dir, &referenced, &SweepOptions::default(), &mut report);
        let mut left: Vec<String> = std::fs::read_dir(dir)
            .expect("dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "manager-v2.pid",
                "s_00000000000b.child",
                "s_00000000000c.child",
                "s_00000000000d.child.bak"
            ]
        );
        assert_eq!(report.removed_files, 1);
        assert_eq!(report.kept_referenced, 1);
        assert_eq!(report.kept_recent, 1);

        remove_holder_record(temp.path(), "s_00000000000b").expect("remove");
        remove_holder_record(temp.path(), "s_00000000000b").expect("missing is fine");
        assert!(!dir.join("s_00000000000b.child").exists());
        if resolved != temp.path() {
            let _ = std::fs::remove_dir_all(&resolved);
        }
    }

    #[test]
    fn private_roots_collapse_nesting_and_never_include_home() {
        let home = Path::new("/home/u");
        let roots = outermost_private_roots(
            home,
            vec![
                "/home/u/.local/state/diri".into(),
                "/home/u/.local/state/diri/run".into(),
                "/home/u/.config/diri".into(),
                "/home/u/.config/diri".into(),
                "/home/u".into(),
                "/home".into(),
                "relative".into(),
            ],
        );
        assert_eq!(
            roots,
            vec![
                std::path::PathBuf::from("/home/u/.config/diri"),
                "/home/u/.local/state/diri".into(),
            ]
        );
        for root in private_roots(home) {
            assert!(
                !home.starts_with(&root),
                "{} would expose the home",
                root.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_tightening_makes_older_world_readable_state_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Dirijor");
        let logs = root.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let checkpoint = logs.join("s_0123456789ab.screen.plist");
        let hosts = root.join("hosts.json");
        std::fs::write(&checkpoint, b"x").unwrap();
        std::fs::write(&hosts, b"{}").unwrap();
        for path in [&root, &logs] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for path in [&checkpoint, &hosts] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let report = tighten_roots(std::slice::from_ref(&root));
        assert_eq!((report.visited, report.changed), (4, 4));
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&logs), 0o700);
        assert_eq!(mode(&checkpoint), 0o600);
        assert_eq!(mode(&hosts), 0o600);
    }

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
