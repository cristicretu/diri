//! The local spool: one background writer thread per process appends JSONL
//! records to `<state>/telemetry/spool/<process>-<pid>-<start_ms>-<n>.open`.
//!
//! Callers never block: records go through a bounded channel and are
//! dropped (and counted) when it is full. The writer sleeps in `recv()`
//! between records, so an idle process never wakes for telemetry. A file is
//! renamed to `.jsonl` when it reaches [`ROLL_BYTES`]; files of processes that
//! exit stay `.open` and the uploader treats a dead writer's file as sealed.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender};

use serde::Serialize;

use crate::value::{Fields, Value};
use crate::{Process, Severity};

pub const ROLL_BYTES: u64 = 2 * 1024 * 1024;
/// Local retention cap for the whole spool directory, uploaded or not.
pub const SPOOL_CAP_BYTES: u64 = 64 * 1024 * 1024;
pub const CHANNEL_CAPACITY: usize = 8192;
pub const OPEN_SUFFIX: &str = "open";
pub const SEALED_SUFFIX: &str = "jsonl";
pub const URGENT_MARKER: &str = "urgent";

pub(crate) struct Record {
    pub t: u64,
    pub seq: u64,
    pub kind: &'static str,
    pub sev: Severity,
    pub fields: Vec<(&'static str, Value)>,
}

pub(crate) enum Message {
    Record(Record),
    Flush(SyncSender<()>),
}

#[derive(Serialize)]
struct Line<'a> {
    t: u64,
    seq: u64,
    p: &'static str,
    pid: u32,
    k: &'static str,
    s: &'static str,
    f: Fields<'a>,
}

#[must_use]
pub fn spool_dir(state_dir: &Path) -> PathBuf {
    crate::identity::telemetry_dir(state_dir).join("spool")
}

pub(crate) fn spawn(dir: PathBuf, process: Process) -> std::io::Result<SyncSender<Message>> {
    crate::identity::create_private_dir(&dir)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(CHANNEL_CAPACITY);
    let writer = Writer {
        dir,
        process,
        pid: std::process::id(),
        start_ms: crate::now_ms(),
        index: 0,
        current: None,
    };
    std::thread::Builder::new()
        .name("diri-telemetry".into())
        .spawn(move || writer.run(&rx))?;
    Ok(tx)
}

struct Writer {
    dir: PathBuf,
    process: Process,
    pid: u32,
    start_ms: u64,
    index: u32,
    current: Option<(PathBuf, BufWriter<File>, u64)>,
}

impl Writer {
    fn run(mut self, rx: &Receiver<Message>) {
        let mut line = Vec::with_capacity(512);
        while let Ok(first) = rx.recv() {
            let mut urgent = false;
            let mut acks = Vec::new();
            let mut message = Some(first);
            while let Some(next) = message.take() {
                match next {
                    Message::Record(record) => {
                        urgent |= record.sev == Severity::Incident;
                        self.write(&record, &mut line);
                    }
                    Message::Flush(ack) => acks.push(ack),
                }
                message = rx.try_recv().ok();
            }
            self.report_dropped(&mut line);
            if let Some((_, file, _)) = self.current.as_mut() {
                let _ = file.flush();
            }
            if urgent {
                let _ = diri_platform::private::create(&self.dir.join(URGENT_MARKER));
            }
            for ack in acks {
                let _ = ack.send(());
            }
        }
    }

    fn report_dropped(&mut self, line: &mut Vec<u8>) {
        let Some(recorder) = crate::RECORDER.get() else {
            return;
        };
        let dropped = recorder.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let record = Record {
                t: crate::now_ms(),
                seq: recorder.seq.fetch_add(1, Ordering::Relaxed),
                kind: "telemetry.dropped",
                sev: Severity::Warn,
                fields: vec![("count", Value::from(dropped))],
            };
            self.write(&record, line);
        }
    }

    fn write(&mut self, record: &Record, line: &mut Vec<u8>) {
        line.clear();
        let entry = Line {
            t: record.t,
            seq: record.seq,
            p: self.process.as_str(),
            pid: self.pid,
            k: record.kind,
            s: record.sev.as_str(),
            f: Fields(&record.fields),
        };
        if serde_json::to_writer(&mut *line, &entry).is_err() {
            return;
        }
        line.push(b'\n');
        if self.current.is_none() && self.open().is_err() {
            return;
        }
        let Some((_, file, size)) = self.current.as_mut() else {
            return;
        };
        if file.write_all(line).is_ok() {
            *size += line.len() as u64;
        }
        if *size >= ROLL_BYTES {
            self.seal();
        }
    }

    fn open(&mut self) -> std::io::Result<()> {
        let path = self.dir.join(format!(
            "{}-{}-{}-{:04}.{OPEN_SUFFIX}",
            self.process.as_str(),
            self.pid,
            self.start_ms,
            self.index
        ));
        self.index += 1;
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&path)?;
        self.current = Some((path, BufWriter::with_capacity(64 * 1024, file), 0));
        Ok(())
    }

    fn seal(&mut self) {
        if let Some((path, mut file, _)) = self.current.take() {
            let _ = file.flush();
            drop(file);
            let _ = std::fs::rename(&path, path.with_extension(SEALED_SUFFIX));
        }
        prune(&self.dir, SPOOL_CAP_BYTES);
    }
}

/// Deletes the oldest sealed files until the directory fits `cap` bytes.
pub fn prune(dir: &Path, cap: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    meta.len(),
                    entry.path(),
                )
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= cap {
            break;
        }
        if path.extension().and_then(|e| e.to_str()) == Some(SEALED_SUFFIX) {
            let _ = std::fs::remove_file(&path);
            total = total.saturating_sub(len);
        }
    }
}

/// Parsed `<process>-<pid>-<start_ms>-<n>.<suffix>` spool file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolName {
    pub process: String,
    pub pid: u32,
    pub start_ms: u64,
    pub index: u32,
    pub sealed: bool,
}

impl SpoolName {
    #[must_use]
    pub fn parse(file_name: &str) -> Option<Self> {
        let (stem, suffix) = file_name.rsplit_once('.')?;
        let sealed = match suffix {
            SEALED_SUFFIX => true,
            OPEN_SUFFIX => false,
            _ => return None,
        };
        let mut parts = stem.split('-');
        let process = parts.next()?.to_owned();
        let pid = parts.next()?.parse().ok()?;
        let start_ms = parts.next()?.parse().ok()?;
        let index = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self {
            process,
            pid,
            start_ms,
            index,
            sealed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spool_names() {
        assert_eq!(
            SpoolName::parse("engine-812-1790581979447-0003.open"),
            Some(SpoolName {
                process: "engine".into(),
                pid: 812,
                start_ms: 1_790_581_979_447,
                index: 3,
                sealed: false
            })
        );
        assert!(SpoolName::parse("app-1-2-3.jsonl").unwrap().sealed);
        assert_eq!(SpoolName::parse("urgent"), None);
        assert_eq!(SpoolName::parse("offsets.json"), None);
    }

    #[test]
    fn prune_removes_oldest_sealed_files_only() {
        let dir = tempfile::tempdir().unwrap();
        for (name, age) in [
            ("a-1-1-0000.jsonl", 3),
            ("a-1-1-0001.jsonl", 2),
            ("a-1-1-0002.open", 5),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![b'x'; 100]).unwrap();
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age * 60);
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        prune(dir.path(), 250);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["a-1-1-0001.jsonl", "a-1-1-0002.open"]);
    }
}
