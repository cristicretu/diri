//! Append-only, offset-addressed log of one session's raw PTY output.
//!
//! Every byte ever produced has a monotonically increasing stream offset. The
//! most recent `ring_capacity` bytes stay in memory; everything is also
//! appended to a spill file capped at `disk_capacity` by half-truncation
//! rewrites. Offsets never reset, so a client that reconnects can always
//! reconcile against "skip to now".
//!
//! # File format
//!
//! This is a port of the Swift `OutputLog`, and the format is deliberately
//! identical — a log written by the Swift holder must be readable here and
//! vice versa, or switching engines would strand live sessions.
//!
//! ```text
//! offset  size  field
//! 0       4     magic 0x4449524A ("DIRJ"), big endian
//! 4       4     version = 1, big endian
//! 8       8     base offset, big endian: stream offset of the first payload byte
//! 16      ..    raw payload bytes
//! ```
//!
//! Not internally synchronized: one owner per session, as with the Swift actor.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x4449_524A;
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 16;
const MAX_SYNC_POINTS: usize = 64;
const SYNC_INTERVAL: Duration = Duration::from_secs(2);

/// Default in-memory window: 4 MiB.
pub const DEFAULT_RING_CAPACITY: usize = 4 << 20;
/// Default spill-file cap per session: 8 MiB, matching `SessionLogStorage`.
pub const DEFAULT_DISK_CAPACITY: usize = 8 << 20;

pub struct OutputLog {
    ring_capacity: usize,
    disk_capacity: usize,

    tail_offset: u64,
    /// Oldest offset still resident in memory.
    ring_start_offset: u64,
    ring: Vec<u8>,

    path: PathBuf,
    read_only: bool,
    write_handle: Option<File>,
    /// Long-lived reader. PTY output can produce hundreds of filesystem events
    /// per second; reopening per chunk dominated log-drain samples in Swift.
    read_handle: Option<File>,
    file_base_offset: u64,
    file_bytes: usize,
    last_sync: Instant,

    /// Offsets where a full-screen reset begins (ESC c or CSI 2J) — clean
    /// replay starting points. Newest last, bounded.
    sync_points: Vec<u64>,
    /// Partial escape prefix carried across chunk boundaries (up to 3 bytes).
    carry: Vec<u8>,
}

impl OutputLog {
    /// Opens (or recovers, or creates) the log for `session_id` under `dir`.
    pub fn open(
        dir: &Path,
        session_id: &str,
        ring_capacity: usize,
        disk_capacity: usize,
        read_only: bool,
    ) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let mut log = Self {
            ring_capacity,
            disk_capacity,
            tail_offset: 0,
            ring_start_offset: 0,
            ring: Vec::new(),
            path: dir.join(format!("{session_id}.bin")),
            read_only,
            write_handle: None,
            read_handle: None,
            file_base_offset: 0,
            file_bytes: 0,
            last_sync: Instant::now(),
            sync_points: Vec::new(),
            carry: Vec::new(),
        };
        log.open_or_recover()?;
        Ok(log)
    }

    /// Writer with the standard capacities.
    pub fn writer(dir: &Path, session_id: &str) -> io::Result<Self> {
        Self::open(
            dir,
            session_id,
            DEFAULT_RING_CAPACITY,
            DEFAULT_DISK_CAPACITY,
            false,
        )
    }

    /// Read-only view of a log another process owns.
    pub fn reader(dir: &Path, session_id: &str) -> io::Result<Self> {
        Self::open(
            dir,
            session_id,
            DEFAULT_RING_CAPACITY,
            DEFAULT_DISK_CAPACITY,
            true,
        )
    }

    /// Where this log lives on disk, for filesystem watchers.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn tail_offset(&self) -> u64 {
        self.tail_offset
    }

    pub fn ring_start_offset(&self) -> u64 {
        self.ring_start_offset
    }

    /// Oldest byte this process can still reconstruct from either the spill
    /// file or its in-memory ring. Cold terminal-mode recovery must start
    /// here, rather than at a latency-oriented tail window: DEC modes are
    /// sticky and their last transition may be arbitrarily far behind the
    /// visible screen.
    pub fn oldest_available_offset(&self) -> u64 {
        self.ring_start_offset
            .min(self.file_base_offset)
            .min(self.tail_offset)
    }

    pub fn sync_points(&self) -> &[u64] {
        &self.sync_points
    }

    // MARK: Append

    /// Appends a chunk and returns the offset range it occupies.
    ///
    /// In-memory state advances even when the disk write fails, so stream
    /// offsets stay monotonic and readers are never handed a rewound tail. The
    /// error is returned rather than swallowed so the caller can report it.
    pub fn append(&mut self, data: &[u8]) -> io::Result<Range<u64>> {
        if data.is_empty() {
            return Ok(self.tail_offset..self.tail_offset);
        }
        let start = self.tail_offset;

        self.scan_for_sync_points(data, start);

        self.ring.extend_from_slice(data);
        if self.ring.len() > self.ring_capacity {
            let drop = self.ring.len() - self.ring_capacity;
            self.ring.drain(..drop);
            self.ring_start_offset += drop as u64;
        }
        self.tail_offset += data.len() as u64;

        self.append_to_disk(data)?;
        Ok(start..self.tail_offset)
    }

    // MARK: Read

    /// Reads up to `max_bytes` from `from_offset`, clamped to what remains
    /// available. Returns the actual start offset and the bytes.
    pub fn read(&mut self, from_offset: u64, max_bytes: usize) -> (u64, Vec<u8>) {
        let start = from_offset.max(self.oldest_available_offset());
        if start >= self.tail_offset {
            return (self.tail_offset, Vec::new());
        }
        let end = self.tail_offset.min(start + max_bytes as u64);

        if !self.read_only
            && start >= self.ring_start_offset
            && end - self.ring_start_offset <= self.ring.len() as u64
        {
            let lower = (start - self.ring_start_offset) as usize;
            let upper = (end - self.ring_start_offset) as usize;
            return (start, self.ring[lower..upper].to_vec());
        }
        self.read_from_disk(start, end)
    }

    /// Best replay start for a byte budget: the newest sync point within
    /// budget, else budget bytes back from the tail.
    pub fn preferred_replay_start(&self, budget: usize) -> u64 {
        let budget = budget as u64;
        let floor = self.tail_offset.saturating_sub(budget);
        if let Some(sync) = self
            .sync_points
            .iter()
            .rev()
            .find(|&&s| s >= floor && s < self.tail_offset)
        {
            return *sync;
        }
        floor.max(
            self.ring_start_offset
                .min(self.file_base_offset)
                .min(self.tail_offset),
        )
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        if let Some(handle) = self.write_handle.as_mut() {
            handle.sync_all()?;
        }
        self.last_sync = Instant::now();
        Ok(())
    }

    /// Refreshes a read-only view after the owner appended or rotated the file.
    /// Returns whether anything changed.
    pub fn refresh_from_disk(&mut self) -> bool {
        if !self.read_only {
            return false;
        }
        let Some(handle) = self.reader_handle() else {
            return false;
        };
        let mut header = [0u8; HEADER_SIZE];
        if handle.seek(SeekFrom::Start(0)).is_err() || handle.read_exact(&mut header).is_err() {
            return false;
        }
        if read_u32_be(&header, 0) != MAGIC {
            return false;
        }
        let new_base = read_u64_be(&header, 8);
        let size = handle.seek(SeekFrom::End(0)).unwrap_or(HEADER_SIZE as u64);
        let new_bytes = (size as usize).saturating_sub(HEADER_SIZE);
        let new_tail = new_base + new_bytes as u64;
        let changed = new_tail != self.tail_offset || new_base != self.file_base_offset;
        self.file_base_offset = new_base;
        self.file_bytes = new_bytes;
        self.tail_offset = new_tail;
        self.sync_points.retain(|&s| s >= new_base);
        changed
    }

    /// File replacement or truncation changes the inode behind a cached
    /// reader; call this before consuming a rename/delete notification.
    pub fn invalidate_read_handle(&mut self) {
        self.read_handle = None;
    }

    // MARK: Sync-point scanning

    fn scan_for_sync_points(&mut self, data: &[u8], chunk_start: u64) {
        // Work over carry + chunk so sequences split across chunks are found.
        let carry_count = self.carry.len();
        let mut haystack = std::mem::take(&mut self.carry);
        haystack.extend_from_slice(data);
        let keep = haystack.len().saturating_sub(3);
        self.carry = haystack[keep..].to_vec();

        if haystack.len() < 2 {
            return;
        }
        for i in 0..haystack.len() - 1 {
            if haystack[i] != 0x1B {
                continue;
            }
            let found = haystack[i + 1] == 0x63 // ESC c — full reset
                || (i + 3 < haystack.len()
                    && haystack[i + 1] == 0x5B
                    && haystack[i + 2] == 0x32
                    && haystack[i + 3] == 0x4A); // CSI 2J — erase display
            if !found {
                continue;
            }
            // A sequence beginning inside the carry region starts before
            // chunk_start — still a valid (earlier) replay start.
            let signed = chunk_start as i64 + (i as i64 - carry_count as i64);
            let offset = signed.max(0) as u64;
            if self.sync_points.last() != Some(&offset) {
                self.sync_points.push(offset);
                if self.sync_points.len() > MAX_SYNC_POINTS {
                    let excess = self.sync_points.len() - MAX_SYNC_POINTS;
                    self.sync_points.drain(..excess);
                }
            }
        }
    }

    // MARK: Disk spill

    fn open_or_recover(&mut self) -> io::Result<()> {
        let existing = File::open(&self.path).ok().and_then(|mut handle| {
            let mut header = [0u8; HEADER_SIZE];
            handle.read_exact(&mut header).ok()?;
            if read_u32_be(&header, 0) != MAGIC {
                return None;
            }
            let base = read_u64_be(&header, 8);
            let size = handle.seek(SeekFrom::End(0)).ok()?;
            Some((handle, base, (size as usize).saturating_sub(HEADER_SIZE)))
        });

        match existing {
            Some((handle, base, bytes)) => {
                self.file_base_offset = base;
                self.file_bytes = bytes;
                self.tail_offset = base + bytes as u64;
                self.ring_start_offset = self.tail_offset;
                if self.read_only {
                    self.read_handle = Some(handle);
                } else {
                    drop(handle);
                    self.write_handle = Some(open_for_append(&self.path)?);
                }
                Ok(())
            }
            None => self.create_file(0),
        }
    }

    fn create_file(&mut self, base_offset: u64) -> io::Result<()> {
        self.invalidate_read_handle();
        let mut header = Vec::with_capacity(HEADER_SIZE);
        header.extend_from_slice(&MAGIC.to_be_bytes());
        header.extend_from_slice(&VERSION.to_be_bytes());
        header.extend_from_slice(&base_offset.to_be_bytes());
        write_private(&self.path, &header)?;
        self.file_base_offset = base_offset;
        self.file_bytes = 0;
        if !self.read_only {
            self.write_handle = Some(open_for_append(&self.path)?);
        }
        Ok(())
    }

    fn append_to_disk(&mut self, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        let Some(handle) = self.write_handle.as_mut() else {
            return Ok(());
        };
        handle.write_all(data)?;
        self.file_bytes += data.len();

        if self.last_sync.elapsed() > SYNC_INTERVAL {
            handle.sync_all()?;
            self.last_sync = Instant::now();
        }

        if self.file_bytes > self.disk_capacity {
            self.truncate_to_half()?;
        }
        Ok(())
    }

    /// Rewrites the spill file keeping only the newest half, bumping the base
    /// offset so stream offsets stay monotonic across the rewrite.
    fn truncate_to_half(&mut self) -> io::Result<()> {
        self.invalidate_read_handle();
        let keep = self.disk_capacity / 2;
        let drop_bytes = self.file_bytes.saturating_sub(keep);

        let mut read = File::open(&self.path)?;
        read.seek(SeekFrom::Start((HEADER_SIZE + drop_bytes) as u64))?;
        let mut kept = Vec::new();
        read.read_to_end(&mut kept)?;
        drop(read);
        self.write_handle = None;

        let new_base = self.file_base_offset + drop_bytes as u64;
        let mut content = Vec::with_capacity(HEADER_SIZE + kept.len());
        content.extend_from_slice(&MAGIC.to_be_bytes());
        content.extend_from_slice(&VERSION.to_be_bytes());
        content.extend_from_slice(&new_base.to_be_bytes());
        content.extend_from_slice(&kept);

        let tmp = self.path.with_extension("tmp");
        write_private(&tmp, &content)?;
        fs::rename(&tmp, &self.path)?;

        self.file_base_offset = new_base;
        self.file_bytes = kept.len();
        self.write_handle = Some(open_for_append(&self.path)?);
        self.sync_points.retain(|&s| s >= new_base);
        Ok(())
    }

    fn read_from_disk(&mut self, from_offset: u64, to_offset: u64) -> (u64, Vec<u8>) {
        let base = self.file_base_offset;
        let tail = self.tail_offset;
        if let Some(handle) = self.write_handle.as_mut() {
            let _ = handle.flush();
        }
        let Some(handle) = self.reader_handle() else {
            return (tail, Vec::new());
        };
        let start = from_offset.max(base);
        let pos = HEADER_SIZE as u64 + (start - base);
        if handle.seek(SeekFrom::Start(pos)).is_err() {
            return (tail, Vec::new());
        }
        let count = (to_offset - start) as usize;
        let mut buffer = vec![0u8; count];
        let mut filled = 0;
        while filled < count {
            match handle.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => break,
            }
        }
        buffer.truncate(filled);
        (start, buffer)
    }

    fn reader_handle(&mut self) -> Option<&mut File> {
        if self.read_handle.is_none() {
            self.read_handle = File::open(&self.path).ok();
        }
        self.read_handle.as_mut()
    }
}

impl Drop for OutputLog {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn open_for_append(path: &Path) -> io::Result<File> {
    let mut handle = OpenOptions::new().write(true).open(path)?;
    handle.seek(SeekFrom::End(0))?;
    Ok(handle)
}

/// Writes owner-only. Session output is the user's terminal content and must
/// not be world-readable.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    Ok(())
}

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64_be(bytes: &[u8], offset: usize) -> u64 {
    let mut value = 0u64;
    for i in 0..8 {
        value = (value << 8) | bytes[offset + i] as u64;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn header_matches_the_swift_on_disk_format() {
        let root = dir();
        let mut log = OutputLog::writer(root.path(), "s").expect("open");
        log.append(b"hello").expect("append");
        log.flush().expect("flush");

        let bytes = fs::read(root.path().join("s.bin")).expect("read");
        assert_eq!(&bytes[0..4], b"DIRJ", "magic is DIRJ in big endian");
        assert_eq!(read_u32_be(&bytes, 4), 1, "version");
        assert_eq!(read_u64_be(&bytes, 8), 0, "base offset starts at zero");
        assert_eq!(&bytes[HEADER_SIZE..], b"hello");
    }

    #[test]
    fn offsets_are_monotonic_across_appends() {
        let root = dir();
        let mut log = OutputLog::writer(root.path(), "s").expect("open");
        assert_eq!(log.append(b"abc").expect("append"), 0..3);
        assert_eq!(log.append(b"de").expect("append"), 3..5);
        assert_eq!(log.tail_offset(), 5);
    }

    #[test]
    fn ring_eviction_serves_older_bytes_from_disk() {
        let root = dir();
        // Ring smaller than the data forces eviction; disk keeps everything.
        let mut log = OutputLog::open(root.path(), "s", 8, 1 << 20, false).expect("open");
        log.append(b"0123456789").expect("append");

        assert!(log.ring_start_offset() > 0, "ring evicted its oldest bytes");
        let (offset, data) = log.read(0, 4);
        assert_eq!(offset, 0, "evicted bytes are still addressable");
        assert_eq!(data, b"0123", "and come back from the spill file");
    }

    #[test]
    fn disk_rotation_keeps_offsets_monotonic() {
        let root = dir();
        let mut log = OutputLog::open(root.path(), "s", 1 << 20, 64, false).expect("open");
        for _ in 0..20 {
            log.append(b"0123456789").expect("append");
        }
        assert_eq!(
            log.tail_offset(),
            200,
            "tail counts every byte ever written"
        );

        // Rotation dropped the oldest bytes but did not rewind the stream.
        let (offset, data) = log.read(190, 10);
        assert_eq!(offset, 190);
        assert_eq!(data, b"0123456789");

        let bytes = fs::read(root.path().join("s.bin")).expect("read");
        let base = read_u64_be(&bytes, 8);
        assert!(base > 0, "rotation advanced the file base offset");
        assert_eq!(
            base + (bytes.len() - HEADER_SIZE) as u64,
            200,
            "base plus payload still lands on the true tail"
        );
    }

    #[test]
    fn sync_points_record_screen_resets() {
        let root = dir();
        let mut log = OutputLog::writer(root.path(), "s").expect("open");
        log.append(b"aaa").expect("append");
        log.append(b"\x1b[2Jfresh").expect("append");

        assert_eq!(log.sync_points(), &[3], "CSI 2J is a replay start");
        assert_eq!(
            log.preferred_replay_start(1 << 20),
            3,
            "replay prefers the newest reset within budget"
        );
    }

    #[test]
    fn sync_points_survive_a_split_escape_sequence() {
        let root = dir();
        let mut log = OutputLog::writer(root.path(), "s").expect("open");
        log.append(b"aa\x1b").expect("append");
        log.append(b"[2Jrest").expect("append");

        assert_eq!(
            log.sync_points(),
            &[2],
            "a sequence split across chunks is found at its true offset"
        );
    }

    #[test]
    fn a_reader_recovers_the_stream_written_by_another_owner() {
        let root = dir();
        {
            let mut writer = OutputLog::writer(root.path(), "s").expect("open");
            writer.append(b"first").expect("append");
            writer.flush().expect("flush");
        }

        let mut reader = OutputLog::reader(root.path(), "s").expect("open");
        assert_eq!(reader.tail_offset(), 5);
        let (offset, data) = reader.read(0, 16);
        assert_eq!(offset, 0);
        assert_eq!(data, b"first");
    }

    #[test]
    fn a_reader_sees_appends_after_refreshing() {
        let root = dir();
        let mut writer = OutputLog::writer(root.path(), "s").expect("open");
        writer.append(b"first").expect("append");
        writer.flush().expect("flush");

        let mut reader = OutputLog::reader(root.path(), "s").expect("open");
        assert_eq!(reader.tail_offset(), 5);

        writer.append(b"-second").expect("append");
        writer.flush().expect("flush");

        assert!(reader.refresh_from_disk(), "refresh reports the growth");
        assert_eq!(reader.tail_offset(), 12);
        let (_, data) = reader.read(5, 16);
        assert_eq!(data, b"-second");
    }

    /// Interop against a log the Swift holder actually wrote.
    ///
    /// Ignored by default because it needs a real log. Point
    /// `DIRI_INTEROP_LOG` at a **copy** of one — never at a live session's
    /// file, which its holder is still appending to:
    ///
    /// ```sh
    /// cp "~/Library/Application Support/Dirijor/logs/s_xxx.bin" /tmp/probe.bin
    /// DIRI_INTEROP_LOG=/tmp/probe.bin cargo test -p diri-engine -- --ignored
    /// ```
    #[test]
    #[ignore = "needs DIRI_INTEROP_LOG pointing at a copy of a Swift-written log"]
    fn reads_a_log_written_by_the_swift_holder() {
        // Skip rather than fail when unconfigured, so `cargo test -- --ignored`
        // runs whichever interop probes this machine can actually do.
        let Ok(raw) = std::env::var("DIRI_INTEROP_LOG") else {
            eprintln!("skipped: DIRI_INTEROP_LOG is not set");
            return;
        };
        let path = PathBuf::from(raw);
        let size = fs::metadata(&path).expect("stat").len();
        let dir = path.parent().expect("parent");
        let id = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .to_string();

        let mut log = OutputLog::reader(dir, &id).expect("open");
        assert_eq!(
            log.tail_offset(),
            size - HEADER_SIZE as u64,
            "tail equals payload length for a log that has never rotated"
        );

        let (offset, data) = log.read(log.tail_offset().saturating_sub(64), 64);
        assert_eq!(offset, log.tail_offset().saturating_sub(64));
        assert_eq!(data.len(), 64, "the last 64 bytes come back intact");
    }

    #[test]
    fn reading_past_the_tail_yields_nothing() {
        let root = dir();
        let mut log = OutputLog::writer(root.path(), "s").expect("open");
        log.append(b"abc").expect("append");

        let (offset, data) = log.read(99, 16);
        assert_eq!(offset, 3, "clamped to the tail");
        assert!(data.is_empty());
    }
}
