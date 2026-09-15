//! Bounded offset-addressed raw PTY output owned by one Holder.
//!
//! Format 3 reuses fixed pages: no rename, unlink, truncate, or fsync on append.
//! Two checksummed commit headers per page preserve the last complete prefix
//! if a process is killed during an append. Format 1 remains readable.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::paths::reject_symlink;

const MAGIC: u32 = 0x4452_4C47; // DRLG
const VERSION: u32 = 3;
const HEADER_BYTES: usize = 16;
pub const DISK_CAPACITY: usize = 32 << 20;
const PAGE_BYTES: usize = 64 << 10;
const PAGE_COUNT: usize = DISK_CAPACITY / PAGE_BYTES;
const COMMIT_BYTES: usize = 64;
const PAGE_STRIDE: usize = PAGE_BYTES + 2 * COMMIT_BYTES;
const COMMIT_MAGIC: u32 = 0x4452_434D; // DRCM

#[derive(Clone, Copy)]
struct Page {
    base: u64,
    len: usize,
    commit: usize,
}

impl Page {
    fn tail(self) -> u64 {
        self.base + self.len as u64
    }
    fn location(self) -> u64 {
        HEADER_BYTES as u64
            + (self.base / PAGE_BYTES as u64 % PAGE_COUNT as u64) * PAGE_STRIDE as u64
    }
    fn payload(self) -> u64 {
        self.location() + (2 * COMMIT_BYTES) as u64
    }
}

pub struct OutputLog {
    writer: File,
    legacy: Option<(u64, usize)>,
    pages: VecDeque<Page>,
    hasher: Sha256,
}

impl OutputLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        reject_symlink(path)?;
        if !path.exists() {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(&MAGIC.to_be_bytes())?;
            file.write_all(&VERSION.to_be_bytes())?;
            file.write_all(&0_u64.to_be_bytes())?;
            file.sync_all()?;
            File::open(
                path.parent()
                    .ok_or_else(|| invalid("missing output log parent"))?,
            )?
            .sync_all()?;
        }
        let mut writer = OpenOptions::new().read(true).write(true).open(path)?;
        let file_len = writer.metadata()?.len();
        if !writer.metadata()?.is_file() {
            return Err(invalid("output log is not a regular file"));
        }
        let mut header = [0_u8; HEADER_BYTES];
        writer.read_exact(&mut header)?;
        if u32::from_be_bytes(header[..4].try_into().unwrap()) != MAGIC {
            return Err(invalid("output log magic is invalid"));
        }
        let version = u32::from_be_bytes(header[4..8].try_into().unwrap());
        if version == 1 {
            let base = u64::from_be_bytes(header[8..].try_into().unwrap());
            let len = file_len - HEADER_BYTES as u64;
            if len > DISK_CAPACITY as u64 || base.checked_add(len).is_none() {
                return Err(invalid("legacy output log exceeds its bounds"));
            }
            return Ok(Self {
                writer,
                legacy: Some((base, len as usize)),
                pages: VecDeque::new(),
                hasher: Sha256::new(),
            });
        }
        if version != VERSION
            || header[8..] != [0; 8]
            || file_len > (HEADER_BYTES + PAGE_COUNT * PAGE_STRIDE) as u64
        {
            return Err(invalid("output log version or size is invalid"));
        }
        let mut pages = Vec::new();
        let mut payload = vec![0_u8; PAGE_BYTES];
        for slot in 0..PAGE_COUNT {
            let location = (HEADER_BYTES + slot * PAGE_STRIDE) as u64;
            if location >= file_len {
                break;
            }
            let mut best = None;
            for commit in 0..2 {
                let mut header = [0_u8; COMMIT_BYTES];
                if let Err(error) =
                    writer.read_exact_at(&mut header, location + (commit * COMMIT_BYTES) as u64)
                {
                    if error.kind() == io::ErrorKind::UnexpectedEof {
                        continue;
                    }
                    return Err(error);
                }
                let base = u64::from_be_bytes(header[..8].try_into().unwrap());
                let len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
                let magic = u32::from_be_bytes(header[12..16].try_into().unwrap());
                if magic != COMMIT_MAGIC
                    || len == 0
                    || len > PAGE_BYTES
                    || !base.is_multiple_of(PAGE_BYTES as u64)
                    || base / PAGE_BYTES as u64 % PAGE_COUNT as u64 != slot as u64
                    || base.checked_add(len as u64).is_none()
                {
                    continue;
                }
                let page = Page { base, len, commit };
                if let Err(error) = writer.read_exact_at(&mut payload[..len], page.payload()) {
                    if error.kind() == io::ErrorKind::UnexpectedEof {
                        continue;
                    }
                    return Err(error);
                }
                let mut hasher = page_hasher(base);
                hasher.update(&payload[..len]);
                if hasher.finalize().as_slice() != &header[16..48] {
                    continue;
                }
                if best.is_none_or(|old: Page| page.tail() > old.tail()) {
                    best = Some(page);
                }
            }
            if let Some(page) = best {
                pages.push(page);
            }
        }
        pages.sort_unstable_by_key(|page| page.base);
        if pages
            .windows(2)
            .any(|pair| pair[0].len != PAGE_BYTES || pair[0].tail() != pair[1].base)
        {
            return Err(invalid("output log has a gap or corrupt interior page"));
        }
        let mut hasher = Sha256::new();
        if let Some(page) = pages.last() {
            hasher = page_hasher(page.base);
            writer.read_exact_at(&mut payload[..page.len], page.payload())?;
            hasher.update(&payload[..page.len]);
        }
        Ok(Self {
            writer,
            legacy: None,
            pages: pages.into(),
            hasher,
        })
    }

    #[must_use]
    pub fn tail_offset(&self) -> u64 {
        self.legacy.map_or_else(
            || self.pages.back().map_or(0, |page| page.tail()),
            |(base, len)| base + len as u64,
        )
    }

    pub fn append(&mut self, mut bytes: &[u8]) -> io::Result<u64> {
        if self.legacy.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "version 1 output logs are read-only",
            ));
        }
        let start = self.tail_offset();
        start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("output offset overflow"))?;
        while !bytes.is_empty() {
            let new_page = self.pages.back().is_none_or(|page| page.len == PAGE_BYTES);
            let mut page = if new_page {
                let base = self.tail_offset();
                self.hasher = page_hasher(base);
                Page {
                    base,
                    len: 0,
                    commit: 1,
                }
            } else {
                *self.pages.back().unwrap()
            };
            let count = bytes.len().min(PAGE_BYTES - page.len);
            self.writer
                .write_all_at(&bytes[..count], page.payload() + page.len as u64)?;
            self.hasher.update(&bytes[..count]);
            page.len += count;
            page.commit ^= 1;
            let mut header = [0_u8; COMMIT_BYTES];
            header[..8].copy_from_slice(&page.base.to_be_bytes());
            header[8..12].copy_from_slice(&(page.len as u32).to_be_bytes());
            header[12..16].copy_from_slice(&COMMIT_MAGIC.to_be_bytes());
            header[16..48].copy_from_slice(self.hasher.clone().finalize().as_slice());
            self.writer.write_all_at(
                &header,
                page.location() + (page.commit * COMMIT_BYTES) as u64,
            )?;
            if new_page {
                if self.pages.len() == PAGE_COUNT {
                    self.pages.pop_front();
                }
                self.pages.push_back(page);
            } else {
                *self.pages.back_mut().unwrap() = page;
            }
            bytes = &bytes[count..];
        }
        Ok(start)
    }

    pub fn read(&self, requested_offset: u64, maximum: usize) -> io::Result<(u64, Vec<u8>)> {
        let base = self.legacy.map_or_else(
            || self.pages.front().map_or(0, |page| page.base),
            |(base, _)| base,
        );
        let start = requested_offset.max(base);
        let tail = self.tail_offset();
        if start >= tail || maximum == 0 {
            return Ok((tail, Vec::new()));
        }
        let count = usize::try_from((tail - start).min(maximum as u64)).unwrap_or(maximum);
        let mut bytes = vec![0_u8; count];
        if self.legacy.is_some() {
            self.writer
                .read_exact_at(&mut bytes, HEADER_BYTES as u64 + start - base)?;
        } else {
            let mut offset = start;
            for page in &self.pages {
                if offset >= page.tail() {
                    continue;
                }
                let count =
                    (page.tail() - offset).min(start + bytes.len() as u64 - offset) as usize;
                let index = (offset - start) as usize;
                self.writer.read_exact_at(
                    &mut bytes[index..index + count],
                    page.payload() + offset - page.base,
                )?;
                offset += count as u64;
                if offset == start + bytes.len() as u64 {
                    break;
                }
            }
        }
        Ok((start, bytes))
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.sync_data()
    }
}

fn page_hasher(base: u64) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(base.to_be_bytes());
    hasher
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "disk latency measurement; run in release mode on the target host"]
    fn append_latency_gate() {
        let temporary = tempfile::tempdir().expect("temp");
        let mut log = OutputLog::open(&temporary.path().join("output.log")).expect("open");
        let chunk = vec![b'x'; 64 << 10];
        let mut timings = Vec::new();
        for index in 0..2048 {
            let start = std::time::Instant::now();
            log.append(&chunk).expect("append");
            timings.push((start.elapsed(), index));
        }
        timings.sort_unstable();
        eprintln!(
            "append 64 KiB: median={:?} p99={:?} max={:?}",
            timings[1024].0, timings[2027].0, timings[2047].0
        );
        eprintln!(
            "slowest append (duration, 64 KiB chunk index; page reuse starts at chunk 512): {:?}",
            &timings[2040..]
        );
        assert!(
            timings[2047].0 < std::time::Duration::from_millis(16),
            "log maintenance blocked the Holder for more than one output frame"
        );
    }

    #[test]
    fn offsets_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut log = OutputLog::open(&path).unwrap();
        assert_eq!(log.append(b"one").unwrap(), 0);
        assert_eq!(log.append(b"two").unwrap(), 3);
        log.flush().unwrap();
        drop(log);
        let log = OutputLog::open(&path).unwrap();
        assert_eq!(log.tail_offset(), 6);
        assert_eq!(log.read(2, 3).unwrap(), (2, b"etw".to_vec()));
    }

    #[test]
    fn replay_survives_wrap_reopen_and_partial_page_append() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let payload: Vec<_> = (0..DISK_CAPACITY + PAGE_BYTES + 37)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut log = OutputLog::open(&path).unwrap();
        assert_eq!(log.append(&payload).unwrap(), 0);
        log.flush().unwrap();
        drop(log);
        let mut log = OutputLog::open(&path).unwrap();
        let base = 2 * PAGE_BYTES;
        assert_eq!(log.tail_offset(), payload.len() as u64);
        assert_eq!(
            log.read(0, usize::MAX).unwrap(),
            (base as u64, payload[base..].to_vec())
        );
        assert!(
            std::fs::metadata(&path).unwrap().len()
                <= (HEADER_BYTES + PAGE_COUNT * PAGE_STRIDE) as u64
        );
        assert_eq!(log.append(b"tail").unwrap(), payload.len() as u64);
        drop(log);
        let log = OutputLog::open(&path).unwrap();
        assert_eq!(
            log.read(payload.len() as u64 - 1, 5).unwrap().1,
            [&payload[payload.len() - 1..], b"tail"].concat()
        );
    }

    #[test]
    fn interrupted_commit_recovers_the_last_complete_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut log = OutputLog::open(&path).unwrap();
        log.append(b"one").unwrap();
        log.append(b"two").unwrap();
        let page = *log.pages.back().unwrap();
        log.writer
            .write_all_at(
                b"partial",
                page.location() + (page.commit * COMMIT_BYTES) as u64,
            )
            .unwrap();
        drop(log);
        let log = OutputLog::open(&path).unwrap();
        assert_eq!(log.read(0, 100).unwrap().1, b"one");
        assert_eq!(log.tail_offset(), 3);
    }

    #[test]
    fn interrupted_reuse_evicts_only_the_oldest_page() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut log = OutputLog::open(&path).unwrap();
        log.append(&vec![b'a'; DISK_CAPACITY]).unwrap();
        let oldest = *log.pages.front().unwrap();
        // The next append is killed after overwriting data but before commit.
        log.writer
            .write_all_at(b"changed", oldest.payload())
            .unwrap();
        drop(log);
        let log = OutputLog::open(&path).unwrap();
        assert_eq!(log.tail_offset(), DISK_CAPACITY as u64);
        assert_eq!(log.read(0, 1).unwrap(), (PAGE_BYTES as u64, b"a".to_vec()));
    }

    #[test]
    fn corrupt_interior_page_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut log = OutputLog::open(&path).unwrap();
        log.append(&vec![b'a'; 3 * PAGE_BYTES]).unwrap();
        log.writer
            .write_all_at(b"bad", log.pages[1].payload())
            .unwrap();
        drop(log);
        assert!(OutputLog::open(&path).is_err());
    }

    #[test]
    fn old_logs_are_readable_without_rewriting_their_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut bytes = MAGIC.to_be_bytes().to_vec();
        bytes.extend(1_u32.to_be_bytes());
        bytes.extend(123_u64.to_be_bytes());
        bytes.extend(b"old");
        std::fs::write(&path, &bytes).unwrap();
        let mut log = OutputLog::open(&path).unwrap();
        assert_eq!(log.tail_offset(), 126);
        assert_eq!(log.read(0, 10).unwrap(), (123, b"old".to_vec()));
        assert_eq!(
            log.append(b"new").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
