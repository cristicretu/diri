//! Encrypted swap for idle terminal history.
//!
//! When a held session has been idle for [`page_out_idle`] (no output and no
//! history reads), its pump moves the session's sealed history payloads out
//! of memory into a [`SwapFile`] (see `HeadlessScreen::page_out_history`).
//! Reads (scroll, Find, copy, a checkpoint) bring them back a payload at a
//! time and keep nothing of them.
//!
//! * **Where.** `<logs>/history-swap/<pid>-<instance>/<random>.swap`. The
//!   directories are `0700` and the files `0600` from creation; a file is
//!   created with `O_EXCL` and every open refuses a symlink. One file per
//!   session, opened only for the duration of one page-out or one read, so
//!   an idle session holds no descriptor.
//! * **Sealing.** Every payload is sealed on its own with AES-256-GCM under
//!   one key per Engine process, drawn from the OS random source and held
//!   only in memory. A record is `counter (8) | ciphertext | tag (16)`; the
//!   nonce is a random per-process prefix plus that counter, which never
//!   repeats in a process, and the file's random id, the record's offset and
//!   its length are authenticated with it, so a record moved, copied or
//!   truncated fails to open.
//! * **Lifetime.** A file is only ever read by the process that wrote it,
//!   at offsets that process recorded after a complete write: nothing a
//!   crash leaves behind is trusted, and without the key it cannot be read.
//!   A file is removed when its session's history is dropped, and
//!   [`sweep_stale`] removes what an Engine that is gone left behind.
//!   History survives an Engine restart through the screen checkpoint, as
//!   before; the swap is a memory extension, never a recovery source.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use diri_terminal_state::ColdStore;

/// The directory under the logs directory that holds every Engine's swap.
pub const DIR_NAME: &str = "history-swap";

/// Bytes a record adds to its payload: the nonce counter and the GCM tag.
pub const RECORD_OVERHEAD: usize = 8 + 16;

/// Most bytes staged before one write while sealing a page-out. Kept well
/// below the size the macOS allocator caches whole after a free, so a
/// page-out leaves no transient allocation resident.
const STAGING_BYTES: usize = 64 << 10;

/// A trailing run of freed records this long is given back to the file
/// system at once; shorter ones wait to be reused.
const TRUNCATE_TAIL: u64 = 1 << 20;

/// How long a held session's history must be idle (no output and no
/// history read) before it is paged out. `DIRI_HISTORY_SWAP_IDLE_MS`
/// overrides it (tests and benchmarks); `DIRI_HISTORY_SWAP=0` turns paging
/// off.
pub fn page_out_idle() -> Option<Duration> {
    static IDLE: OnceLock<Option<Duration>> = OnceLock::new();
    *IDLE.get_or_init(|| {
        if std::env::var_os("DIRI_HISTORY_SWAP").is_some_and(|value| value == "0") {
            return None;
        }
        let millis = std::env::var("DIRI_HISTORY_SWAP_IDLE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60_000);
        Some(Duration::from_millis(millis))
    })
}

/// The per-process sealing key and nonce prefix, or `None` when the OS
/// random source failed (paging is then off).
struct Seal {
    cipher: aes_gcm::Aes256Gcm,
    prefix: [u8; 4],
}

fn seal() -> Option<&'static Seal> {
    static SEAL: OnceLock<Option<Seal>> = OnceLock::new();
    SEAL.get_or_init(|| {
        use aes_gcm::KeyInit;
        let mut key = [0u8; 32];
        let mut prefix = [0u8; 4];
        getrandom::fill(&mut key).ok()?;
        getrandom::fill(&mut prefix).ok()?;
        let cipher = aes_gcm::Aes256Gcm::new(&key.into());
        key.fill(0);
        Some(Seal { cipher, prefix })
    })
    .as_ref()
}

/// Nonce counter shared by every file in the process: a nonce is never
/// used twice under the process key.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_u64() -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("no OS randomness"))?;
    Ok(u64::from_le_bytes(bytes))
}

/// This process's directory name: its pid and a random instance, so a
/// later process that reuses the pid never mistakes it for its own.
fn instance() -> Option<&'static str> {
    static INSTANCE: OnceLock<Option<String>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            random_u64()
                .ok()
                .map(|random| format!("{}-{:08x}", std::process::id(), random as u32))
        })
        .as_deref()
}

fn nonce(prefix: [u8; 4], counter: u64) -> aes_gcm::Nonce<aes_gcm::aead::consts::U12> {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&prefix);
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce.into()
}

fn associated(file: u64, offset: u64, len: usize) -> [u8; 24] {
    let mut aad = [0u8; 24];
    aad[..8].copy_from_slice(&file.to_be_bytes());
    aad[8..16].copy_from_slice(&offset.to_be_bytes());
    aad[16..].copy_from_slice(&(len as u64).to_be_bytes());
    aad
}

/// Free space in one swap file: extents by offset, and the file's end.
#[derive(Default)]
struct Extents {
    end: u64,
    free: BTreeMap<u64, u64>,
    live: u64,
    exists: bool,
}

impl Extents {
    /// The first free extent that fits `size`, or the end of the file.
    fn allocate(&mut self, size: u64) -> u64 {
        let found = self
            .free
            .iter()
            .find(|&(_, &len)| len >= size)
            .map(|(&at, &len)| (at, len));
        self.live += size;
        if let Some((at, len)) = found {
            self.free.remove(&at);
            if len > size {
                self.free.insert(at + size, len - size);
            }
            return at;
        }
        let at = self.end;
        self.end += size;
        at
    }

    /// Frees `[at, at + size)`, joining it with free neighbours. Returns
    /// the new end when a long free tail was cut off the file.
    fn release(&mut self, mut at: u64, mut size: u64) -> Option<u64> {
        self.live = self.live.saturating_sub(size);
        if let Some((&before, &len)) = self.free.range(..at).next_back()
            && before + len == at
        {
            self.free.remove(&before);
            at = before;
            size += len;
        }
        if let Some(len) = self.free.remove(&(at + size)) {
            size += len;
        }
        if at + size == self.end && size >= TRUNCATE_TAIL {
            self.end = at;
            return Some(at);
        }
        self.free.insert(at, size);
        None
    }
}

/// One session's encrypted, owner-only swap file; see the module docs.
pub struct SwapFile {
    path: PathBuf,
    /// Authenticated with every record, so records never verify in another
    /// file.
    id: u64,
    extents: Mutex<Extents>,
    /// What one [`Reading`] holds: a read descriptor, so a whole-history
    /// read (a checkpoint) opens the file once rather than per block, and
    /// the last record it opened, which the next block's decode often asks
    /// for again (read batches do not end on block boundaries).
    reader: Mutex<Option<HeldReader>>,
}

struct HeldReader {
    file: std::fs::File,
    last: Option<(u64, Vec<u8>)>,
}

/// Keeps a [`SwapFile`] open for reading until dropped.
pub struct Reading(Arc<SwapFile>);

impl Reading {
    /// Holds `swap` open for reading until the guard drops: for a reader
    /// about to load many records in a row (a checkpoint export reads up to
    /// 10,000 rows). Without one, every record opens the file, so an idle
    /// session keeps no descriptor.
    pub fn new(swap: Arc<SwapFile>) -> Reading {
        #[cfg(unix)]
        if swap.extents.lock().is_ok_and(|extents| extents.exists)
            && let Ok(file) = swap.open(false, false)
            && let Ok(mut reader) = swap.reader.lock()
        {
            *reader = Some(HeldReader { file, last: None });
        }
        Reading(swap)
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        if let Ok(mut reader) = self.0.reader.lock() {
            reader.take();
        }
    }
}

impl std::fmt::Debug for SwapFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwapFile")
            .field("path", &self.path)
            .finish()
    }
}

impl SwapFile {
    /// A swap file for one session under `logs_dir`. Nothing is created on
    /// disk until the first page-out stores something.
    pub fn new(logs_dir: &Path) -> io::Result<Arc<SwapFile>> {
        if seal().is_none() {
            return Err(io::Error::other("no sealing key"));
        }
        let instance = instance().ok_or_else(|| io::Error::other("no OS randomness"))?;
        let id = random_u64()?;
        let path = logs_dir
            .join(DIR_NAME)
            .join(instance)
            .join(format!("{id:016x}.swap"));
        Ok(Arc::new(SwapFile {
            path,
            id,
            extents: Mutex::new(Extents::default()),
            reader: Mutex::new(None),
        }))
    }

    /// Where the file is (or will be).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes of live records, framing included.
    pub fn live_bytes(&self) -> u64 {
        self.extents.lock().map_or(0, |extents| extents.live)
    }

    /// The file's length as this process laid it out.
    pub fn file_bytes(&self) -> u64 {
        self.extents.lock().map_or(0, |extents| extents.end)
    }

    /// Opens the file: created (`create`), for writing, or read-only. Never
    /// through a symlink.
    #[cfg(unix)]
    fn open(&self, create: bool, write: bool) -> io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        if create {
            let directory = self.path.parent().expect("swap directory");
            ensure_private_directory(directory.parent().expect("swap root"))?;
            ensure_private_directory(directory)?;
            let file = diri_platform::private::options()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&self.path)?;
            return Ok(file);
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
    }

    #[cfg(unix)]
    fn store_all(&self, payloads: &[&[u8]]) -> io::Result<Vec<u64>> {
        use aes_gcm::aead::AeadInOut;
        use std::os::unix::fs::FileExt;
        let seal = seal().ok_or_else(|| io::Error::other("no sealing key"))?;
        let mut extents = self.extents.lock().expect("swap extents");
        let file = self.open(!extents.exists, true)?;
        extents.exists = true;
        let mut slots = Vec::with_capacity(payloads.len());
        let mut staging: Vec<u8> = Vec::with_capacity(STAGING_BYTES);
        let mut staged_at = 0u64;
        let mut result = Ok(());
        for payload in payloads {
            let size = (payload.len() + RECORD_OVERHEAD) as u64;
            let at = extents.allocate(size);
            slots.push(at);
            let contiguous = staged_at + staging.len() as u64 == at;
            if !staging.is_empty() && (!contiguous || staging.len() + size as usize > STAGING_BYTES)
            {
                result = file.write_all_at(&staging, staged_at);
                staging.clear();
                if result.is_err() {
                    break;
                }
            }
            if staging.is_empty() {
                staged_at = at;
            }
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            staging.extend_from_slice(&counter.to_be_bytes());
            let start = staging.len();
            staging.extend_from_slice(payload);
            let tag = seal
                .cipher
                .encrypt_inout_detached(
                    &nonce(seal.prefix, counter),
                    &associated(self.id, at, payload.len()),
                    (&mut staging[start..]).into(),
                )
                .map_err(|_| io::Error::other("seal failed"));
            match tag {
                Ok(tag) => staging.extend_from_slice(&tag),
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        if result.is_ok() && !staging.is_empty() {
            result = file.write_all_at(&staging, staged_at);
        }
        if let Err(error) = result {
            // Nothing was handed out: every extent this call took is free.
            for (slot, payload) in slots.iter().zip(payloads) {
                extents.release(*slot, (payload.len() + RECORD_OVERHEAD) as u64);
            }
            let _ = file.set_len(extents.end);
            return Err(error);
        }
        Ok(slots)
    }

    #[cfg(unix)]
    fn load_one(&self, slot: u64, len: usize) -> io::Result<Vec<u8>> {
        use aes_gcm::aead::AeadInOut;
        use std::os::unix::fs::FileExt;
        let seal = seal().ok_or_else(|| io::Error::other("no sealing key"))?;
        let mut held = self.reader.lock().ok();
        let held = held.as_deref_mut().and_then(Option::as_mut);
        if let Some(HeldReader {
            last: Some((at, bytes)),
            ..
        }) = &held
            && *at == slot
        {
            return Ok(bytes.clone());
        }
        let mut record = vec![0u8; len + RECORD_OVERHEAD];
        match &held {
            Some(reader) => reader.file.read_exact_at(&mut record, slot)?,
            None => self.open(false, false)?.read_exact_at(&mut record, slot)?,
        }
        let counter = u64::from_be_bytes(record[..8].try_into().expect("8 bytes"));
        let (body, tag) = record[8..].split_at_mut(len);
        let tag = aes_gcm::Tag::try_from(&*tag).expect("16-byte tag");
        seal.cipher
            .decrypt_inout_detached(
                &nonce(seal.prefix, counter),
                &associated(self.id, slot, len),
                body.into(),
                &tag,
            )
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "swap record failed to open")
            })?;
        record.copy_within(8..8 + len, 0);
        record.truncate(len);
        if let Some(reader) = held {
            reader.last = Some((slot, record.clone()));
        }
        Ok(record)
    }

    #[cfg(not(unix))]
    fn store_all(&self, _payloads: &[&[u8]]) -> io::Result<Vec<u64>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    #[cfg(not(unix))]
    fn load_one(&self, _slot: u64, _len: usize) -> io::Result<Vec<u8>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

impl ColdStore for SwapFile {
    fn store(&self, payloads: &[&[u8]]) -> io::Result<Vec<u64>> {
        self.store_all(payloads)
    }

    fn load(&self, slot: u64, len: usize) -> io::Result<Vec<u8>> {
        self.load_one(slot, len).inspect_err(|error| {
            diri_telemetry::warn_event!(
                "session.history_swap_read_failed",
                io = diri_telemetry::io_error(error),
                bytes = len,
            );
        })
    }

    fn release(&self, slot: u64, len: usize) {
        let Ok(mut extents) = self.extents.lock() else {
            return;
        };
        let cut = extents.release(slot, (len + RECORD_OVERHEAD) as u64);
        if extents.live == 0 && extents.exists {
            // Nothing left: remove the file rather than keep an empty one.
            let _ = std::fs::remove_file(&self.path);
            *extents = Extents::default();
        } else if let Some(end) = cut {
            #[cfg(unix)]
            if let Ok(file) = self.open(false, true) {
                let _ = file.set_len(end);
            }
            #[cfg(not(unix))]
            let _ = end;
        }
    }
}

impl Drop for SwapFile {
    fn drop(&mut self) {
        let exists = self
            .extents
            .get_mut()
            .map_or(true, |extents| extents.exists);
        if exists {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Creates `path` owner-only if missing, and refuses one that is a symlink,
/// not a directory, or not owned by this user. A directory that is ours but
/// readable by others is made owner-only.
#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match diri_platform::private::create_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "swap directory is not a private directory",
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

/// What [`sweep_stale`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapSweep {
    pub removed_dirs: usize,
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub kept_live: usize,
}

/// Removes swap directories whose Engine is gone: their key died with it,
/// so nothing in them can be read again. A directory is this process's own
/// (kept), another live process's (kept: a second Engine runs), or stale.
/// Only names of the form `<pid>-<hex>` are touched, symlinks are removed
/// as links and never followed, and only regular files are deleted.
pub fn sweep_stale(logs_dir: &Path) -> SwapSweep {
    let mut report = SwapSweep::default();
    let root = logs_dir.join(DIR_NAME);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return report;
    };
    let own = instance();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = parse_instance(name) else {
            continue;
        };
        if Some(name) == own {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        if pid != std::process::id() && process_alive(pid) {
            report.kept_live += 1;
            continue;
        }
        if let Ok(files) = std::fs::read_dir(entry.path()) {
            for file in files.flatten() {
                let Ok(metadata) = std::fs::symlink_metadata(file.path()) else {
                    continue;
                };
                if (metadata.is_file() || metadata.file_type().is_symlink())
                    && std::fs::remove_file(file.path()).is_ok()
                    && metadata.is_file()
                {
                    report.removed_files += 1;
                    report.removed_bytes += metadata.len();
                }
            }
        }
        if std::fs::remove_dir(entry.path()).is_ok() {
            report.removed_dirs += 1;
        }
    }
    report
}

/// The pid of a directory named `<pid>-<hex>`.
fn parse_instance(name: &str) -> Option<u32> {
    let (pid, random) = name.split_once('-')?;
    if pid.is_empty()
        || random.is_empty()
        || !pid.bytes().all(|b| b.is_ascii_digit())
        || !random.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    pid.parse().ok()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn records_round_trip_owner_only_and_the_file_goes_with_its_history() {
        let temp = tempfile::tempdir().unwrap();
        let swap = SwapFile::new(temp.path()).unwrap();
        let payloads: Vec<Vec<u8>> = (0..300)
            .map(|n| (0..(n * 37 % 4000 + 1)).map(|b| (b * n) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
        let slots = swap.store(&refs).unwrap();
        assert_eq!(slots.len(), payloads.len());
        assert_eq!(mode(swap.path()), 0o600);
        assert_eq!(mode(swap.path().parent().unwrap()), 0o700);
        assert_eq!(mode(&temp.path().join(DIR_NAME)), 0o700);
        for (slot, payload) in slots.iter().zip(&payloads) {
            assert_eq!(&swap.load(*slot, payload.len()).unwrap(), payload);
        }
        // Nothing on disk is plaintext.
        let disk = std::fs::read(swap.path()).unwrap();
        assert!(
            !disk
                .windows(64)
                .any(|window| window == &payloads[299][..64]),
            "payloads are sealed"
        );
        // Freed space is reused before the file grows.
        let end = swap.file_bytes();
        swap.release(slots[10], payloads[10].len());
        let again = swap.store(&[&payloads[10][..]]).unwrap();
        assert_eq!(again, vec![slots[10]]);
        assert_eq!(swap.file_bytes(), end);
        for (slot, payload) in slots.iter().zip(&payloads) {
            swap.release(*slot, payload.len());
        }
        assert!(!swap.path().exists(), "an empty swap is removed");
        let path = swap.path().to_path_buf();
        swap.store(&[&payloads[1][..]]).unwrap();
        assert!(path.exists());
        drop(swap);
        assert!(!path.exists(), "dropped with its history");
    }

    #[test]
    fn a_tampered_moved_or_truncated_record_fails_to_open() {
        let temp = tempfile::tempdir().unwrap();
        let swap = SwapFile::new(temp.path()).unwrap();
        let a = vec![7u8; 500];
        let b = vec![9u8; 500];
        let slots = swap.store(&[&a, &b]).unwrap();
        let mut disk = std::fs::read(swap.path()).unwrap();
        // The same length at another offset: authenticated with its offset.
        assert!(swap.load(slots[1], a.len()).is_ok());
        let flipped = slots[0] as usize + 8 + 100;
        disk[flipped] ^= 1;
        std::fs::write(swap.path(), &disk).unwrap();
        assert_eq!(
            swap.load(slots[0], a.len()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // Record 1 copied over record 0: right bytes, wrong place.
        let record = (slots[1] - slots[0]) as usize;
        disk.copy_within(
            slots[1] as usize..slots[1] as usize + record,
            slots[0] as usize,
        );
        std::fs::write(swap.path(), &disk).unwrap();
        assert!(swap.load(slots[0], a.len()).is_err());
        // Another file's records never open here.
        let other = SwapFile::new(temp.path()).unwrap();
        let copied = other.path().to_path_buf();
        std::fs::copy(swap.path(), &copied).unwrap();
        other.extents.lock().unwrap().exists = true;
        assert!(other.load(slots[1], b.len()).is_err());
        // Truncated: a short read.
        std::fs::OpenOptions::new()
            .write(true)
            .open(swap.path())
            .unwrap()
            .set_len(slots[1] + 10)
            .unwrap();
        assert!(swap.load(slots[1], b.len()).is_err());
    }

    #[test]
    fn a_symlink_in_place_of_the_swap_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let swap = SwapFile::new(temp.path()).unwrap();
        let directory = swap.path().parent().unwrap();
        diri_platform::private::create_dir_all(directory).unwrap();
        let target = temp.path().join("elsewhere");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, swap.path()).unwrap();
        assert!(swap.store(&[b"secret"]).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_file(swap.path()).unwrap();

        // A symlinked instance directory is refused too.
        let other = SwapFile::new(&temp.path().join("b")).unwrap();
        let root = temp.path().join("b").join(DIR_NAME);
        diri_platform::private::create_dir_all(&root).unwrap();
        let elsewhere = temp.path().join("dir");
        diri_platform::private::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, other.path().parent().unwrap()).unwrap();
        assert!(other.store(&[b"secret"]).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn a_failed_write_hands_out_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let swap = SwapFile::new(temp.path()).unwrap();
        swap.store(&[&[1u8; 100][..]]).unwrap();
        // The file is gone: opening it for the write fails.
        std::fs::remove_file(swap.path()).unwrap();
        swap.extents.lock().unwrap().exists = true;
        assert!(swap.store(&[&[2u8; 100][..]]).is_err());
        assert_eq!(swap.live_bytes(), (100 + RECORD_OVERHEAD) as u64);
    }

    #[test]
    fn the_sweep_removes_only_dead_engines_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(DIR_NAME);
        // This process's own directory, live.
        let swap = SwapFile::new(temp.path()).unwrap();
        swap.store(&[b"mine"]).unwrap();
        // A dead Engine's (pid 0x7fff_fff0 is never a live process).
        let dead = root.join("2147483632-0badc0de");
        diri_platform::private::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("0123456789abcdef.swap"), b"sealed").unwrap();
        // An earlier process with this pid, crashed.
        let reused = root.join(format!("{}-00000001", std::process::id()));
        diri_platform::private::create_dir_all(&reused).unwrap();
        std::fs::write(reused.join("a.swap"), b"sealed").unwrap();
        // A live process other than this one (our parent).
        let parent = root.join(format!("{}-00000002", std::os::unix::process::parent_id()));
        diri_platform::private::create_dir_all(&parent).unwrap();
        // Not ours to touch.
        let foreign = root.join("notes");
        diri_platform::private::create_dir_all(&foreign).unwrap();

        let report = sweep_stale(temp.path());
        assert_eq!(report.removed_dirs, 2);
        assert_eq!(report.removed_files, 2);
        assert_eq!(report.kept_live, 1);
        assert!(!dead.exists() && !reused.exists());
        assert!(parent.exists() && foreign.exists());
        assert!(swap.path().exists(), "this Engine's swap is kept");
        assert_eq!(swap.load(0, 4).unwrap(), b"mine");
    }

    /// What reading paged-out history back costs per 64-row block, with and
    /// without a held descriptor (a checkpoint reads ~157 blocks for 10,000
    /// rows). `cargo test --release -p diri-engine --lib record_load_cost
    /// -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn record_load_cost() {
        let temp = tempfile::tempdir().unwrap();
        let swap = SwapFile::new(temp.path()).unwrap();
        let payloads: Vec<Vec<u8>> = (0..157)
            .map(|n| (0..2_300).map(|b| (b * 31 + n) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
        let started = std::time::Instant::now();
        let slots = swap.store(&refs).unwrap();
        let stored = started.elapsed();
        let mut per_open = Vec::new();
        let mut held = Vec::new();
        for _ in 0..20 {
            let started = std::time::Instant::now();
            for (slot, payload) in slots.iter().zip(&payloads) {
                swap.load(*slot, payload.len()).unwrap();
            }
            per_open.push(started.elapsed());
            let started = std::time::Instant::now();
            {
                let _reading = Reading::new(Arc::clone(&swap));
                for (slot, payload) in slots.iter().zip(&payloads) {
                    swap.load(*slot, payload.len()).unwrap();
                }
            }
            held.push(started.elapsed());
        }
        per_open.sort();
        held.sort();
        println!(
            "157 records of 2.3 KB: store {stored:?}; load all, open per record p50 {:?} p95 {:?}; held descriptor p50 {:?} p95 {:?}",
            per_open[10], per_open[19], held[10], held[19]
        );
    }

    #[test]
    fn extents_coalesce_and_cut_a_long_free_tail() {
        let mut extents = Extents::default();
        let a = extents.allocate(100);
        let b = extents.allocate(TRUNCATE_TAIL);
        let c = extents.allocate(50);
        assert_eq!((a, b, c), (0, 100, 100 + TRUNCATE_TAIL));
        assert_eq!(extents.release(b, TRUNCATE_TAIL), None);
        assert_eq!(extents.release(c, 50), Some(100), "the tail is cut");
        assert_eq!(extents.end, 100);
        assert!(extents.free.is_empty());
        assert_eq!(extents.allocate(10), 100);
    }
}
