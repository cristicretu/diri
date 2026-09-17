//! Stream local rollouts so long conversations do not require whole-file buffers.
use super::*;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_LINE: u64 = 64 * 1024 * 1024;

fn error(message: &str) -> ControlError {
    ControlError::bad_request(message)
}

fn open(location: &Location) -> Result<Option<fs::File>, ControlError> {
    if !check_directories(location, false)? {
        return Ok(None);
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(location.path())
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(error("Cannot safely open the Codex transcript")),
    };
    let meta = file.metadata().map_err(io_control_error)?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(error(
            "Codex transcript must be a regular file owned by you",
        ));
    }
    if meta.len() > MAX_BYTES {
        return Err(error(
            "Codex transcript exceeds the 4 GiB local transfer limit",
        ));
    }
    Ok(Some(file))
}

fn validate(
    file: &mut fs::File,
    source: &SessionRecord,
    mut output: Option<&mut fs::File>,
) -> Result<(), ControlError> {
    file.seek(SeekFrom::Start(0)).map_err(io_control_error)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut total = 0;
    let mut identity = false;
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)
            .map_err(io_control_error)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_BYTES || count as u64 > MAX_LINE {
            return Err(error(
                "Codex transcript exceeds the 4 GiB file or 64 MiB JSON-line transfer limit",
            ));
        }
        if line.last() != Some(&b'\n') {
            return Err(error(
                "Codex transcript has an incomplete final line; wait for its current turn to finish",
            ));
        }
        if line != b"\n" {
            let value: Value = serde_json::from_slice(&line)
                .map_err(|_| error("Codex transcript contains invalid JSON"))?;
            if value["type"] == "session_meta" {
                if identity
                    || value["payload"]["id"].as_str() != source.agent_session_id.as_deref()
                    || value["payload"]["cwd"].as_str() != Some(source.cwd.as_str())
                {
                    return Err(error(
                        "Codex transcript belongs to another conversation or folder",
                    ));
                }
                identity = true;
            }
        }
        if let Some(out) = output.as_mut() {
            out.write_all(&line).map_err(io_control_error)?;
        }
    }
    if !identity {
        return Err(missing_transcript());
    }
    Ok(())
}

fn prefix(source: &mut fs::File, target: &Location) -> Result<Option<fs::Metadata>, ControlError> {
    let Some(mut destination) = open(target)? else {
        return Ok(None);
    };
    let before = destination.metadata().map_err(io_control_error)?;
    source.seek(SeekFrom::Start(0)).map_err(io_control_error)?;
    let mut left = [0; 64 * 1024];
    let mut right = [0; 64 * 1024];
    let mut remaining = before.len();
    while remaining > 0 {
        let n = remaining.min(left.len() as u64) as usize;
        destination
            .read_exact(&mut left[..n])
            .map_err(io_control_error)?;
        if source.read_exact(&mut right[..n]).is_err() || left[..n] != right[..n] {
            return Err(error(
                "The destination account has a different version of this conversation; its history will not be overwritten",
            ));
        }
        remaining -= n as u64;
    }
    if !same_file(&before, &destination.metadata().map_err(io_control_error)?) {
        return Err(error(
            "Destination transcript changed during the account switch",
        ));
    }
    Ok(Some(before))
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

pub(super) fn preflight(
    source: &Location,
    target: &Location,
    record: &SessionRecord,
) -> Result<(), ControlError> {
    let mut file = open(source)?.ok_or_else(missing_transcript)?;
    validate(&mut file, record, None)?;
    prefix(&mut file, target)?;
    Ok(())
}

pub(super) fn install(
    source: &Location,
    target: &Location,
    record: &SessionRecord,
) -> Result<(), ControlError> {
    let mut file = open(source)?.ok_or_else(missing_transcript)?;
    let before = file.metadata().map_err(io_control_error)?;
    if source.path() == target.path() {
        return validate(&mut file, record, None);
    }
    check_directories(target, true)?;
    let temporary = target
        .directory()
        .join(format!(".diri-account-{}.tmp", crate::inject::uuid_v4()));
    let result = (|| {
        let mut staged = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(io_control_error)?;
        validate(&mut file, record, Some(&mut staged))?;
        if !same_file(&before, &file.metadata().map_err(io_control_error)?) {
            return Err(error("Source transcript changed during the account switch"));
        }
        staged.sync_all().map_err(io_control_error)?;
        let expected = prefix(&mut staged, target)?;
        let current = open(target)?
            .map(|f| f.metadata())
            .transpose()
            .map_err(io_control_error)?;
        if !match (&expected, &current) {
            (None, None) => true,
            (Some(a), Some(b)) => same_file(a, b),
            _ => false,
        } {
            return Err(error(
                "Destination transcript changed during the account switch",
            ));
        }
        fs::rename(&temporary, target.path()).map_err(io_control_error)?;
        fs::File::open(target.directory())
            .and_then(|f| f.sync_all())
            .map_err(io_control_error)
    })();
    let _ = fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn streamed_copy_rejects_conflicts_invalid_identity_and_unsafe_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = Location {
            root: temp.path().join("source"),
            relative: "sessions/2026/09/17/rollout-test.jsonl".into(),
        };
        let target = Location {
            root: temp.path().join("target"),
            relative: source.relative.clone(),
        };
        let mut record = super::super::super::tests::test_record("test");
        record.kind = AgentKind::CODEX;
        record.agent_session_id = Some("conversation".into());
        let bytes = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"conversation","cwd":record.cwd}})
        );
        check_directories(&source, true).unwrap();
        fs::write(source.path(), &bytes).unwrap();
        install(&source, &target, &record).unwrap();
        assert_eq!(fs::read(target.path()).unwrap(), bytes.as_bytes());
        fs::write(target.path(), b"divergent\n").unwrap();
        assert!(preflight(&source, &target, &record).is_err());
        assert!(install(&source, &target, &record).is_err());
        assert_eq!(fs::read(target.path()).unwrap(), b"divergent\n");
        fs::remove_file(target.path()).unwrap();
        record.agent_session_id = Some("wrong".into());
        assert!(install(&source, &target, &record).is_err());
        assert!(!target.path().exists());
        record.agent_session_id = Some("conversation".into());
        fs::write(source.path(), bytes.trim_end()).unwrap();
        assert!(install(&source, &target, &record).is_err());
        fs::write(source.path(), &bytes).unwrap();
        symlink(source.path(), target.path()).unwrap();
        assert!(install(&source, &target, &record).is_err());
        assert_eq!(fs::read(source.path()).unwrap(), bytes.as_bytes());
        assert!(
            fs::read_dir(target.directory()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        fs::remove_file(target.path()).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(source.path())
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(preflight(&source, &target, &record).is_err());
        fs::write(source.path(), vec![b' '; MAX_LINE as usize + 1]).unwrap();
        assert!(preflight(&source, &target, &record).is_err());
    }
}
