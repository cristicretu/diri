//! Identity-pinned reads. Capture under Registry; perform all I/O after release.
use super::*;
use diri_proto::process_facts::ProcessFacts;
use std::io;
use std::sync::atomic::AtomicUsize;

static ACTIVE_READS: AtomicUsize = AtomicUsize::new(0);
struct Admission;
impl Admission {
    fn acquire() -> io::Result<Self> {
        ACTIVE_READS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 4).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "process inspection capacity reached",
                )
            })
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        ACTIVE_READS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct ProcessFactsReader {
    shared: Arc<Shared>,
    transport: Transport,
}
impl Session {
    pub(crate) fn process_facts_reader(&self) -> ProcessFactsReader {
        let transport = match &self.transport {
            Transport::Direct(pty) => Transport::Direct(Arc::clone(pty)),
            Transport::Held(holder) => Transport::Held(holder.clone()),
            Transport::Remote(client) => Transport::Remote(Arc::clone(client)),
        };
        ProcessFactsReader {
            shared: Arc::clone(&self.shared),
            transport,
        }
    }
}
impl ProcessFactsReader {
    pub(crate) fn matches(&self, session: &Session) -> bool {
        Arc::ptr_eq(&self.shared, &session.shared)
            && !self.shared.stop.load(Ordering::SeqCst)
            && !self.shared.exited.load(Ordering::SeqCst)
    }
    fn check_live(&self, deadline: Instant) -> io::Result<()> {
        diri_pty::unix_socket::remaining(deadline)?;
        if self.shared.exited.load(Ordering::SeqCst) || self.shared.stop.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "session child is no longer available",
            ));
        }
        Ok(())
    }
    pub(crate) fn read(&self, deadline: Instant) -> io::Result<ProcessFacts> {
        self.check_live(deadline)?;
        let _admission = Admission::acquire()?;
        let facts = match &self.transport {
            Transport::Remote(client) => client.process_facts(deadline)?,
            Transport::Direct(pty) => {
                let identity = pty
                    .try_lock()
                    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "session PTY is busy"))?
                    .child_identity()
                    .ok_or_else(unsupported)?;
                inspect_local(&identity, deadline)?
            }
            Transport::Held(holder) => {
                let expected = self.shared.holder_identity.get().ok_or_else(unsupported)?;
                let before = holder.stat_until(deadline)?;
                let identity = bound_identity(&before)?;
                if (identity, before.epoch_offset.expect("validated epoch")) != *expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Holder no longer matches captured session child",
                    ));
                }
                let facts = inspect_local(&identity, deadline)?;
                let after = holder.stat_until(deadline)?;
                if bound_identity(&after)? != identity || before.epoch_offset != after.epoch_offset
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Holder child changed during inspection",
                    ));
                }
                facts
            }
        };
        self.check_live(deadline)?;
        let captured_pid = self.shared.child_pid.load(Ordering::SeqCst);
        if captured_pid > 0 && facts.identity.pid() != captured_pid as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process facts no longer match session child",
            ));
        }
        facts
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(facts)
    }
}
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "owner does not provide a captured child identity",
    )
}
fn bound_identity(stat: &HolderStat) -> io::Result<diri_proto::process::ProcessIdentity> {
    if !stat.alive {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Holder child is not live",
        ));
    }
    if stat.epoch_offset.is_none() {
        return Err(unsupported());
    }
    stat.verified_child_identity().ok_or_else(unsupported)
}
fn inspect_local(
    identity: &diri_proto::process::ProcessIdentity,
    deadline: Instant,
) -> io::Result<ProcessFacts> {
    diri_pty::unix_socket::remaining(deadline)?;
    let executable = HolderLauncher::default_executable_path();
    let facts = diri_pty::process_facts::inspect(identity, |uid| {
        diri_pty::process_facts::account::lookup_until(
            &executable,
            uid,
            deadline.min(Instant::now() + Duration::from_millis(250)),
        )
    })?;
    diri_pty::unix_socket::remaining(deadline)?;
    Ok(facts)
}

/// Captured only at launch/adoption, never refreshed from an inspection request.
pub(super) fn capture_holder(shared: &Shared, stat: &HolderStat) {
    shared.child_pid.store(stat.child_pid, Ordering::SeqCst);
    if let (Some(identity), Some(epoch)) = (stat.verified_child_identity(), stat.epoch_offset) {
        let _ = shared.holder_identity.set((identity, epoch));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn fixture(
        temp: &Path,
        identity: diri_proto::process::ProcessIdentity,
        epoch: u64,
    ) -> (Session, HolderStat) {
        let spec = SessionSpec {
            id: "process-facts-fixture".into(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/"),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: temp.into(),
            holder: None,
            remote: None,
            defer_launch: false,
        };
        let shared = new_shared(
            &spec,
            OutputLog::writer(temp, &spec.id).unwrap(),
            &ManifestEngine::new(Vec::new()),
            true,
        );
        let stat = HolderStat {
            child_identity: Some(identity),
            child_pid: identity.pid() as i32,
            alive: true,
            log_offset: epoch,
            foreground_pid: None,
            cols: Some(80),
            rows: Some(24),
            epoch_offset: Some(epoch),
            secret_input: None,
        };
        capture_holder(&shared, &stat);
        (
            Session {
                shared,
                transport: Transport::Held(HolderClient::at(&temp.join("facts.sock"))),
                pump: None,
                manifest_id: spec.manifest_id,
                deferred: None,
            },
            stat,
        )
    }
    fn serve(listener: UnixListener, replies: Vec<HolderStat>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            for stat in replies {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&mut socket).read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["op"], "stat");
                serde_json::to_writer(&mut socket, &serde_json::json!({"ok":true,"stat":stat}))
                    .unwrap();
                socket.write_all(b"\n").unwrap();
            }
        })
    }
    #[test]
    fn process_facts_preserve_activity_and_require_the_captured_holder_epoch() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let identity = diri_pty::process_identity::observe(std::process::id()).unwrap();
        let (session, stat) = fixture(temp.path(), identity, 42);
        let listener = UnixListener::bind(temp.path().join("facts.sock")).unwrap();
        let server = serve(listener, vec![stat.clone(), stat.clone()]);
        let hot = session.shared.last_hot.load(Ordering::SeqCst);
        let version = session.state_version();
        let reader = session.process_facts_reader();
        let facts = reader
            .read(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(facts.identity, identity);
        assert!(reader.matches(&session));
        assert_eq!(session.shared.last_hot.load(Ordering::SeqCst), hot);
        assert_eq!(session.shared.last_interaction.load(Ordering::SeqCst), 0);
        assert_eq!(session.state_version(), version);
        server.join().unwrap();
        std::fs::remove_file(temp.path().join("facts.sock")).unwrap();
        let listener = UnixListener::bind(temp.path().join("facts.sock")).unwrap();
        let mut replacement = stat;
        replacement.epoch_offset = Some(43);
        capture_holder(&session.shared, &replacement); // OnceLock must retain original binding.
        let server = serve(listener, vec![replacement]);
        assert_eq!(
            reader
                .read(Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        server.join().unwrap();
        session.shared.stop.store(true, Ordering::SeqCst);
        assert!(!reader.matches(&session));
        assert_eq!(
            reader
                .read(Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotConnected
        );
    }
    #[test]
    fn old_holder_and_changed_second_observation_fail_closed() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let identity = diri_pty::process_identity::observe(std::process::id()).unwrap();
        let (session, mut stat) = fixture(temp.path(), identity, 42);
        let listener = UnixListener::bind(temp.path().join("facts.sock")).unwrap();
        let before = stat.clone();
        stat.epoch_offset = Some(43);
        let server = serve(listener, vec![before.clone(), stat]);
        let error = session
            .process_facts_reader()
            .read(Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        server.join().unwrap();
        let mut old = before.clone();
        old.child_identity = None;
        assert_eq!(
            bound_identity(&old).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        old = before;
        old.epoch_offset = None;
        assert_eq!(
            bound_identity(&old).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
