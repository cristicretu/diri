//! Durable segment chains: where a thread changed account or Agent.
//!
//! Tier 1 model switches are visible in the transcript itself (every
//! request records its model), so they are derived by
//! [`diri_proto::thread::segment_chain`] rather than stored. Account
//! handoffs (Tier 2) and cross-Agent handoffs (Tier 3) happen outside the
//! transcript and are recorded here, one owner-only JSON file per session.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use diri_proto::SessionRecord;
use diri_proto::thread::{SegmentCause, ThreadSegment};

/// Segments kept per session; older ones fold into the first.
const MAX_SEGMENTS: usize = 256;

#[derive(Clone, Debug)]
pub struct SegmentStore {
    dir: PathBuf,
}

impl SegmentStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("threads"),
        }
    }

    fn path(&self, session_id: &str) -> Option<PathBuf> {
        let safe = !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        safe.then(|| self.dir.join(format!("{session_id}.json")))
    }

    /// The stored chain, or a single `Start` segment describing `record`.
    pub fn chain(&self, record: &SessionRecord) -> Vec<ThreadSegment> {
        let stored = self
            .path(&record.id.0)
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<ThreadSegment>>(&bytes).ok())
            .unwrap_or_default();
        if stored.is_empty() {
            vec![start_segment(record)]
        } else {
            stored
        }
    }

    /// Appends a boundary at `start_offset`. Idempotent: an identical last
    /// segment is not repeated, so a retried handoff records one divider.
    pub fn append(
        &self,
        record_before: &SessionRecord,
        mut segment: ThreadSegment,
    ) -> io::Result<Vec<ThreadSegment>> {
        let Some(path) = self.path(&record_before.id.0) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid session id",
            ));
        };
        let mut chain = self.chain(record_before);
        if let Some(last) = chain.last()
            && last.kind == segment.kind
            && last.session_id == segment.session_id
            && last.account == segment.account
            && last.start_offset == segment.start_offset
        {
            return Ok(chain);
        }
        segment.index = chain.last().map_or(0, |last| last.index + 1);
        chain.push(segment);
        if chain.len() > MAX_SEGMENTS {
            chain.drain(1..chain.len() - MAX_SEGMENTS + 1);
        }
        write_private(&self.dir, &path, &serde_json::to_vec(&chain)?)?;
        Ok(chain)
    }

    pub fn remove(&self, session_id: &str) {
        if let Some(path) = self.path(session_id) {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn start_segment(record: &SessionRecord) -> ThreadSegment {
    ThreadSegment {
        index: 0,
        kind: record.effective_kind().clone(),
        session_id: record.id.clone(),
        model: None,
        account: record
            .account_profile
            .as_ref()
            .map(|profile| profile.label.clone()),
        start_offset: 0,
        started_at_ms: record.created_at.0 as i64,
        cause: SegmentCause::Start,
    }
}

fn write_private(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SessionRecord {
        serde_json::from_value(serde_json::json!({
            "id": "s-1", "kind": "claude-code", "cwd": "/tmp", "projectID": "p",
            "title": "t", "titleSource": serde_json::to_value(diri_proto::TitleSource::UserRename).unwrap(), "status": serde_json::to_value(diri_proto::SessionStatus::Idle).unwrap(),
            "resumability": serde_json::to_value(diri_proto::Resumability::Resumable).unwrap(), "createdAt": 1000.0, "updatedAt": 1000.0, "pinned": false
        }))
        .expect("record fixture")
    }

    #[test]
    fn a_fresh_session_has_one_start_segment_and_appends_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentStore::new(dir.path());
        let record = record();
        assert_eq!(store.chain(&record).len(), 1);
        let boundary = ThreadSegment {
            account: Some("Work".into()),
            start_offset: 4096,
            cause: SegmentCause::Account,
            ..start_segment(&record)
        };
        let chain = store.append(&record, boundary.clone()).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1].index, 1);
        assert_eq!(store.append(&record, boundary).unwrap().len(), 2);
        assert_eq!(store.chain(&record).len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.path().join("threads/s-1.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
