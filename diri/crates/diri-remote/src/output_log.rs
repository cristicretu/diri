//! Bounded offset-addressed raw PTY output owned by one Holder.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::paths::reject_symlink;

const MAGIC: u32 = 0x4452_4C47; // DRLG
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 16;
pub const DISK_CAPACITY: usize = 32 << 20;

pub struct OutputLog {
    path: PathBuf,
    writer: File,
    base_offset: u64,
    payload_bytes: usize,
}

impl OutputLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        reject_symlink(path)?;
        if !path.exists() {
            write_new(path, 0, &[])?;
        }
        let mut reader = File::open(path)?;
        let mut header = [0_u8; HEADER_BYTES];
        reader.read_exact(&mut header)?;
        if u32::from_be_bytes(header[0..4].try_into().expect("slice")) != MAGIC
            || u32::from_be_bytes(header[4..8].try_into().expect("slice")) != VERSION
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "output log header is invalid",
            ));
        }
        let base_offset = u64::from_be_bytes(header[8..16].try_into().expect("slice"));
        let payload_bytes = usize::try_from(reader.metadata()?.len())
            .unwrap_or(usize::MAX)
            .saturating_sub(HEADER_BYTES);
        let writer = OpenOptions::new()
            .append(true)
            .read(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer,
            base_offset,
            payload_bytes,
        })
    }

    #[must_use]
    pub fn tail_offset(&self) -> u64 {
        self.base_offset + self.payload_bytes as u64
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<u64> {
        let start = self.tail_offset();
        self.writer.write_all(bytes)?;
        self.payload_bytes = self.payload_bytes.saturating_add(bytes.len());
        if self.payload_bytes > DISK_CAPACITY {
            self.truncate_to_half()?;
        }
        Ok(start)
    }

    pub fn read(&self, requested_offset: u64, maximum: usize) -> io::Result<(u64, Vec<u8>)> {
        let start = requested_offset.max(self.base_offset);
        let tail = self.tail_offset();
        if start >= tail || maximum == 0 {
            return Ok((tail, Vec::new()));
        }
        let count = usize::try_from((tail - start).min(maximum as u64)).unwrap_or(maximum);
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(
            HEADER_BYTES as u64 + start.saturating_sub(self.base_offset),
        ))?;
        let mut bytes = vec![0_u8; count];
        file.read_exact(&mut bytes)?;
        Ok((start, bytes))
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.sync_data()
    }

    fn truncate_to_half(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        let keep = DISK_CAPACITY / 2;
        let drop_bytes = self.payload_bytes.saturating_sub(keep);
        let mut source = File::open(&self.path)?;
        source.seek(SeekFrom::Start((HEADER_BYTES + drop_bytes) as u64))?;
        let mut payload = Vec::with_capacity(keep);
        source.read_to_end(&mut payload)?;
        let new_base = self.base_offset + drop_bytes as u64;
        let temporary = self.path.with_extension(format!("tmp-{}", random_hex()?));
        reject_symlink(&temporary)?;
        let replace = (|| {
            write_new(&temporary, new_base, &payload)?;
            fs::rename(&temporary, &self.path)
        })();
        if replace.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        replace?;
        self.writer = OpenOptions::new()
            .append(true)
            .read(true)
            .mode(0o600)
            .open(&self.path)?;
        self.base_offset = new_base;
        self.payload_bytes = payload.len();
        Ok(())
    }
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("secure random source failed: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_new(path: &Path, base_offset: u64, payload: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&MAGIC.to_be_bytes())?;
    file.write_all(&VERSION.to_be_bytes())?;
    file.write_all(&base_offset.to_be_bytes())?;
    file.write_all(payload)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_survive_reopen() {
        let temporary = tempfile::tempdir().expect("temp");
        let path = temporary.path().join("output.log");
        {
            let mut log = OutputLog::open(&path).expect("open");
            assert_eq!(log.append(b"one").expect("append"), 0);
            assert_eq!(log.append(b"two").expect("append"), 3);
            log.flush().expect("flush");
        }
        let log = OutputLog::open(&path).expect("reopen");
        assert_eq!(log.tail_offset(), 6);
        assert_eq!(log.read(2, 3).expect("read"), (2, b"etw".to_vec()));
    }
}
