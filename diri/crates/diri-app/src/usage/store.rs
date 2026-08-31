use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use diri_proto::paths::DirijorPaths;

use super::{
    cache::{self, UsageCacheFile, UsageFileEntry},
    model::{UsageHourAgg, UsageSnapshot, UsageTotals},
    parser::{parse_claude, parse_codex, tail_hash},
    timestamp::days_from_civil,
};

const RETENTION_DAYS: i64 = 35;
const BLOCK_HOURS: i64 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageProvider {
    Claude,
    Codex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanPaths {
    pub roots: Vec<(PathBuf, UsageProvider)>,
    pub cache_file: PathBuf,
}

impl ScanPaths {
    #[must_use]
    pub fn for_home(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        Self {
            roots: vec![
                (home.join(".claude/projects"), UsageProvider::Claude),
                (home.join(".config/claude/projects"), UsageProvider::Claude),
                (home.join(".codex/sessions"), UsageProvider::Codex),
            ],
            cache_file: DirijorPaths::usage_cache_file(home),
        }
    }
}

impl Default for ScanPaths {
    fn default() -> Self {
        let home = env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
        Self::for_home(home)
    }
}

/// One clock sample plus the local-calendar boundaries corresponding to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockReading {
    pub unix_seconds: i64,
    pub today_started_at: i64,
    pub month_started_at: i64,
}

pub trait Clock {
    fn read(&self) -> ClockReading;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn read(&self) -> ClockReading {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            });
        let (today_started_at, month_started_at) = local_window_starts(now);
        ClockReading {
            unix_seconds: now,
            today_started_at,
            month_started_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefreshStats {
    pub files_discovered: u64,
    pub files_unchanged: u64,
    pub files_parsed: u64,
    pub bytes_parsed: u64,
}

pub struct UsageStore<C = SystemClock> {
    paths: ScanPaths,
    clock: C,
    ledger: TranscriptLedger,
    last_stats: RefreshStats,
}

impl UsageStore<SystemClock> {
    #[must_use]
    pub fn new() -> Self {
        Self::with_paths_and_clock(ScanPaths::default(), SystemClock)
    }
}

impl Default for UsageStore<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> UsageStore<C> {
    #[must_use]
    pub fn with_paths_and_clock(paths: ScanPaths, clock: C) -> Self {
        let ledger = TranscriptLedger::load(&paths.cache_file);
        Self {
            paths,
            clock,
            ledger,
            last_stats: RefreshStats {
                files_discovered: 0,
                files_unchanged: 0,
                files_parsed: 0,
                bytes_parsed: 0,
            },
        }
    }

    /// Incrementally scan transcript tails and atomically persist the cache.
    #[must_use]
    pub fn refresh(&mut self) -> UsageSnapshot {
        let reading = self.clock.read();
        let (snapshot, stats) = self.ledger.refresh(&self.paths, reading, None);
        self.last_stats = stats;
        snapshot
    }

    /// Refresh only transcript paths reported by the filesystem watcher. The
    /// initial `refresh` remains a full reconciliation; normal app activity
    /// never needs to walk every historical transcript directory again.
    #[must_use]
    pub fn refresh_paths(&mut self, invalidated: &[PathBuf]) -> UsageSnapshot {
        let reading = self.clock.read();
        let (snapshot, stats) = self.ledger.refresh(&self.paths, reading, Some(invalidated));
        self.last_stats = stats;
        snapshot
    }

    #[must_use]
    pub const fn last_stats(&self) -> RefreshStats {
        self.last_stats
    }

    pub(crate) fn watch_roots(&self) -> Vec<PathBuf> {
        self.paths
            .roots
            .iter()
            .map(|(root, _)| root.clone())
            .collect()
    }
}

/// Owns the durable transcript ledger across refreshes.
///
/// Callers only request a refresh; cache loading, append validation, bounded
/// retention, parsing, and conditional persistence stay behind this seam.
struct TranscriptLedger {
    cache: UsageCacheFile,
    dirty: bool,
}

impl TranscriptLedger {
    fn load(path: &Path) -> Self {
        Self {
            cache: cache::load(path),
            dirty: false,
        }
    }

    fn refresh(
        &mut self,
        paths: &ScanPaths,
        reading: ClockReading,
        invalidated: Option<&[PathBuf]>,
    ) -> (UsageSnapshot, RefreshStats) {
        let (snapshot, stats, changed) = scan(paths, reading, &mut self.cache, invalidated);
        self.dirty |= changed;
        if self.dirty && cache::save(&paths.cache_file, &self.cache).is_ok() {
            self.dirty = false;
        }
        (snapshot, stats)
    }
}

pub struct UsageFormat;

impl UsageFormat {
    #[must_use]
    pub fn money(value: f64) -> String {
        if value >= 100.0 {
            format!("${value:.0}")
        } else {
            format!("${value:.2}")
        }
    }

    #[must_use]
    pub fn tokens(count: i64) -> String {
        let value = count as f64;
        if value >= 1_000_000_000.0 {
            format!("{:.1}B", value / 1_000_000_000.0)
        } else if value >= 1_000_000.0 {
            format!("{:.1}M", value / 1_000_000.0)
        } else if value >= 1_000.0 {
            format!("{:.1}K", value / 1_000.0)
        } else {
            count.to_string()
        }
    }

    #[must_use]
    pub fn remaining(until: i64, now: i64) -> String {
        let minutes = ((until - now) / 60).max(0);
        let hours = minutes / 60;
        let remainder = minutes % 60;
        if hours > 0 {
            format!("{hours}h {remainder}m")
        } else {
            format!("{remainder}m")
        }
    }
}

fn scan(
    paths: &ScanPaths,
    reading: ClockReading,
    cache: &mut UsageCacheFile,
    invalidated: Option<&[PathBuf]>,
) -> (UsageSnapshot, RefreshStats, bool) {
    let cutoff_hour = reading.unix_seconds / 3_600 - RETENTION_DAYS * 24;
    let mut seen_all = cache
        .seen
        .values()
        .flatten()
        .copied()
        .collect::<HashSet<_>>();

    let full_scan = invalidated.is_none();
    let mut transcripts = invalidated.map_or_else(
        || transcript_files(&paths.roots),
        |changed| {
            changed
                .iter()
                .filter_map(|path| {
                    provider_for_path(path, &paths.roots).map(|provider| (path.clone(), provider))
                })
                .filter(|(path, _)| {
                    path.extension().is_some_and(|ext| ext == "jsonl")
                        || cache
                            .files
                            .contains_key(&path.to_string_lossy().into_owned())
                })
                .collect()
        },
    );
    transcripts.sort_by(|left, right| left.0.cmp(&right.0));
    transcripts.dedup_by(|left, right| left.0 == right.0);
    let discovered_paths = transcripts
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect::<HashSet<_>>();
    if full_scan
        && cache.files.keys().any(|path| {
            is_provider_path(Path::new(path), UsageProvider::Claude, &paths.roots)
                && !discovered_paths.contains(path)
        })
    {
        reset_provider(cache, UsageProvider::Claude, &paths.roots);
        let (snapshot, stats, _) = scan(paths, reading, cache, None);
        return (snapshot, stats, true);
    }

    let mut stats = RefreshStats {
        files_discovered: u64::try_from(transcripts.len()).unwrap_or(u64::MAX),
        ..RefreshStats::default()
    };
    let mut live_files = HashSet::new();
    let mut changed = false;

    for (path, provider) in transcripts {
        let key = path.to_string_lossy().into_owned();
        let Ok(metadata) = fs::metadata(&path) else {
            if cache.files.contains_key(&key) {
                if provider == UsageProvider::Claude {
                    reset_provider(cache, UsageProvider::Claude, &paths.roots);
                    let (snapshot, stats, _) = scan(paths, reading, cache, None);
                    return (snapshot, stats, true);
                }
                cache.files.remove(&key);
                changed = true;
            }
            continue;
        };
        live_files.insert(key.clone());
        let revision = FileRevision::from_metadata(&metadata);
        let modified_hour =
            i64::try_from(revision.modified_ns / 3_600_000_000_000).unwrap_or(i64::MAX);

        let mut entry = cache.files.remove(&key);
        let previous_entry = entry.clone();
        if entry
            .as_ref()
            .is_some_and(|existing| revision.is_unchanged(existing))
        {
            stats.files_unchanged += 1;
            cache.files.insert(key, entry.expect("entry was checked"));
            continue;
        }
        if let Some(existing) = entry.as_ref()
            && !revision.is_append_compatible(existing, &path)
        {
            if provider == UsageProvider::Claude {
                reset_provider(cache, UsageProvider::Claude, &paths.roots);
                let (snapshot, stats, _) = scan(paths, reading, cache, None);
                return (snapshot, stats, true);
            }
            entry = None;
            changed = true;
        }
        if entry.is_none() {
            if modified_hour < cutoff_hour {
                continue;
            }
            entry = Some(UsageFileEntry::empty(
                revision.size,
                revision.modified_ns,
                revision.device,
                revision.inode,
            ));
        }

        let mut live = entry.expect("entry initialized above");
        let parsed = match provider {
            UsageProvider::Claude => parse_claude(
                &path,
                live.offset,
                cutoff_hour,
                &mut live.hours,
                &mut seen_all,
                &mut cache.seen,
            ),
            UsageProvider::Codex => parse_codex(
                &path,
                live.offset,
                cutoff_hour,
                &mut live.hours,
                &mut live.model,
            ),
        };
        let Ok(consumed) = parsed else {
            if let Some(previous) = previous_entry {
                cache.files.insert(key, previous);
            }
            continue;
        };
        live.offset += consumed;
        live.size = revision.size;
        live.modified_ns = revision.modified_ns;
        live.device = revision.device;
        live.inode = revision.inode;
        live.tail_hash = tail_hash(&path, live.offset).unwrap_or(0);
        stats.files_parsed += 1;
        stats.bytes_parsed += consumed;
        changed = true;
        cache.files.insert(key, live);
    }

    for entry in cache.files.values_mut() {
        let hours_before_retain = entry.hours.len();
        entry.hours.retain(|hour, _| *hour >= cutoff_hour);
        changed |= hours_before_retain != entry.hours.len();
    }
    if full_scan {
        let files_before_retain = cache.files.len();
        cache.files.retain(|path, entry| {
            live_files.contains(path)
                && (!entry.hours.is_empty()
                    || entry.modified_ns / 3_600_000_000_000
                        >= u64::try_from(cutoff_hour).unwrap_or_default())
        });
        changed |= files_before_retain != cache.files.len();
    }
    let seen_before_retain = cache.seen.len();
    cache.seen.retain(|hour, _| *hour >= cutoff_hour);
    changed |= seen_before_retain != cache.seen.len();

    let mut claude_hours = BTreeMap::new();
    let mut codex_hours = BTreeMap::new();
    for (path, entry) in &cache.files {
        let Some(provider) = provider_for_path(Path::new(path), &paths.roots) else {
            continue;
        };
        let destination = match provider {
            UsageProvider::Claude => &mut claude_hours,
            UsageProvider::Codex => &mut codex_hours,
        };
        for (&hour, &aggregate) in &entry.hours {
            destination
                .entry(hour)
                .or_insert_with(UsageHourAgg::default)
                .merge(aggregate);
        }
    }

    (
        snapshot(&claude_hours, &codex_hours, reading),
        stats,
        changed,
    )
}

#[derive(Clone, Copy, Debug)]
struct FileRevision {
    size: u64,
    modified_ns: u64,
    device: Option<u64>,
    inode: Option<u64>,
}

impl FileRevision {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| {
                u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
            });
        let (device, inode) = file_identity(metadata);
        Self {
            size: metadata.len(),
            modified_ns,
            device,
            inode,
        }
    }

    fn is_unchanged(self, entry: &UsageFileEntry) -> bool {
        self.size == entry.size
            && self.modified_ns == entry.modified_ns
            && self.has_same_identity(entry)
    }

    fn is_append_compatible(self, entry: &UsageFileEntry, path: &Path) -> bool {
        self.size >= entry.offset
            && self.has_same_identity(entry)
            && tail_hash(path, entry.offset).is_ok_and(|hash| hash == entry.tail_hash)
    }

    fn has_same_identity(self, entry: &UsageFileEntry) -> bool {
        match (self.device, self.inode, entry.device, entry.inode) {
            (Some(device), Some(inode), Some(old_device), Some(old_inode)) => {
                device == old_device && inode == old_inode
            }
            _ => true,
        }
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> (Option<u64>, Option<u64>) {
    use std::os::unix::fs::MetadataExt;

    (Some(metadata.dev()), Some(metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_: &fs::Metadata) -> (Option<u64>, Option<u64>) {
    (None, None)
}

fn reset_provider(
    cache: &mut UsageCacheFile,
    provider: UsageProvider,
    roots: &[(PathBuf, UsageProvider)],
) {
    cache
        .files
        .retain(|path, _| !is_provider_path(Path::new(path), provider, roots));
    if provider == UsageProvider::Claude {
        cache.seen.clear();
    }
}

fn is_provider_path(
    path: &Path,
    provider: UsageProvider,
    roots: &[(PathBuf, UsageProvider)],
) -> bool {
    roots
        .iter()
        .any(|(root, root_provider)| *root_provider == provider && path.starts_with(root))
}

fn provider_for_path(path: &Path, roots: &[(PathBuf, UsageProvider)]) -> Option<UsageProvider> {
    roots
        .iter()
        .find_map(|(root, provider)| path.starts_with(root).then_some(*provider))
}

fn transcript_files(roots: &[(PathBuf, UsageProvider)]) -> Vec<(PathBuf, UsageProvider)> {
    let mut files = Vec::new();
    for (root, provider) in roots {
        walk(root, *provider, &mut files);
    }
    files
}

fn walk(root: &Path, provider: UsageProvider, files: &mut Vec<(PathBuf, UsageProvider)>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(&path, provider, files);
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push((path, provider));
        }
    }
}

fn snapshot(
    claude_hours: &BTreeMap<i64, UsageHourAgg>,
    codex_hours: &BTreeMap<i64, UsageHourAgg>,
    reading: ClockReading,
) -> UsageSnapshot {
    let mut result = UsageSnapshot {
        updated_at: reading.unix_seconds,
        ..UsageSnapshot::default()
    };
    let today_start_hour = reading.today_started_at / 3_600;
    let month_start_hour = reading.month_started_at / 3_600;

    for (&hour, &aggregate) in claude_hours {
        if hour >= today_start_hour {
            result.claude.today += aggregate;
        }
        if hour >= month_start_hour {
            result.claude.month += aggregate;
        }
    }
    for (&hour, &aggregate) in codex_hours {
        if hour >= today_start_hour {
            result.codex.today += aggregate;
        }
        if hour >= month_start_hour {
            result.codex.month += aggregate;
        }
    }

    let mut block_start = None;
    let mut block_totals = UsageTotals::default();
    for (&hour, &aggregate) in claude_hours {
        if block_start.is_some_and(|start| hour < start + BLOCK_HOURS) {
            block_totals += aggregate;
        } else {
            block_start = Some(hour);
            block_totals = UsageTotals::default();
            block_totals += aggregate;
        }
    }
    if let Some(start) = block_start {
        let end = (start + BLOCK_HOURS) * 3_600;
        if reading.unix_seconds < end {
            result.claude.session = block_totals;
            result.session_cost = Some(block_totals.cost);
            result.session_started_at = Some(start * 3_600);
            result.session_ends_at = Some(end);
            result.session_remaining_seconds = Some(end - reading.unix_seconds);
        }
    }
    result
}

#[cfg(all(unix, target_pointer_width = "64"))]
fn local_window_starts(now: i64) -> (i64, i64) {
    use std::ffi::{c_char, c_int, c_long};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: c_long,
        tm_zone: *const c_char,
    }

    unsafe extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
        fn mktime(value: *mut Tm) -> i64;
    }

    let mut local = std::mem::MaybeUninit::<Tm>::uninit();
    // SAFETY: `now` and `local` are valid pointers for the duration of the call;
    // this Unix implementation uses a 64-bit `time_t`, guaranteed by the cfg.
    let result = unsafe { localtime_r(&raw const now, local.as_mut_ptr()) };
    if result.is_null() {
        return utc_window_starts(now);
    }
    // SAFETY: `localtime_r` returned non-null and initialized `local`.
    let local = unsafe { local.assume_init() };

    let mut today = local;
    today.tm_hour = 0;
    today.tm_min = 0;
    today.tm_sec = 0;
    today.tm_isdst = -1;

    let mut month = today;
    month.tm_mday = 1;
    // SAFETY: both values originated from `localtime_r`; the edited fields are
    // valid civil times and `mktime` normalizes timezone/DST details.
    let today = unsafe { mktime(&raw mut today) };
    // SAFETY: same reasoning as the preceding `mktime` call.
    let month = unsafe { mktime(&raw mut month) };
    (today, month)
}

#[cfg(not(all(unix, target_pointer_width = "64")))]
fn local_window_starts(now: i64) -> (i64, i64) {
    utc_window_starts(now)
}

fn utc_window_starts(now: i64) -> (i64, i64) {
    let day = now.div_euclid(86_400);
    let (year, month, _) = civil_from_days(day);
    (day * 86_400, days_from_civil(year, month, 1) * 86_400)
}

fn civil_from_days(days: i64) -> (i32, i32, i32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        i32::try_from(year).unwrap_or(if year < 0 { i32::MIN } else { i32::MAX }),
        i32::try_from(month).expect("month is in range"),
        i32::try_from(day).expect("day is in range"),
    )
}
