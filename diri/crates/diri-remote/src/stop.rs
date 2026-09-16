//! Explicit stop uses the existing authenticated controller protocol. This
//! management process never signals a numeric PID or holds a metadata lock
//! while waiting for the Holder to record its child's actual exit.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use diri_proto::remote_pty::{
    Hello, HelloAck, ProtocolVersion, RemoteCapability, RemoteCodec,
    RemoteManagementFailure as Failure, RemoteMessage, RemoteProcessState, RemoteRole,
    STOP_SESSION_PROTOCOL_MINOR, SessionInspection, SessionSelector, StopSession,
};

use crate::paths::{SessionPaths, StatePaths, open_private_file};
use crate::state::{self, SessionState};

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn kill(selector: &SessionSelector) -> io::Result<SessionInspection> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    let roots = StatePaths::resolve()?;
    let paths = roots.session(&selector.session_id)?;
    let expected = loop {
        let lock = lock_until(&paths, deadline)?;
        let expected = read_authenticated(&paths, selector)?;
        let owned = state::holder_lock_held(&paths.lock)?;
        if !owned {
            return match expected.process_state {
                RemoteProcessState::Exited { .. } => {
                    remaining(deadline)?;
                    Ok(expected.inspection())
                }
                RemoteProcessState::Running { .. } => {
                    Err(Failure::HolderUnavailable.into_io_error())
                }
            };
        }
        if expected.holder_build_id != crate::BUILD_ID
            || (matches!(expected.process_state, RemoteProcessState::Running { .. })
                && expected.child_identity.is_none())
        {
            return Err(Failure::StopUnsupported.into_io_error());
        }
        // Verify the captured birth on this host. If the child was just reaped,
        // release the metadata lock so its real exit checkpoint can complete.
        if matches!(expected.process_state, RemoteProcessState::Running { .. }) {
            match crate::inspect_at(&roots, selector) {
                Ok(inspection)
                    if inspection.verified_child_identity() == expected.child_identity => {}
                Ok(_) => return Err(Failure::StopIdentityMismatch.into_io_error()),
                Err(error)
                    if error
                        .get_ref()
                        .and_then(|inner| inner.downcast_ref::<Failure>())
                        == Some(&Failure::ProcessIdentityUnavailable) =>
                {
                    drop(lock);
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                    pause_until(deadline);
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        drop(lock);
        break expected;
    };

    let mut stream = connect_until(&paths.socket, deadline)?;
    let hello = RemoteMessage::Hello(Hello {
        protocol: ProtocolVersion::CURRENT,
        local_build_id: crate::BUILD_ID.into(),
        session_id: expected.session_id.clone(),
        session_token: selector.session_token.clone(),
        expected_incarnation: Some(expected.session_incarnation.clone()),
        requested_role: RemoteRole::Controller,
        client_nonce: state::random_hex(16)?,
        required_capabilities: vec![
            RemoteCapability::ControllerLease,
            RemoteCapability::ProcessIdentity,
            RemoteCapability::StopSession,
        ],
        last_acknowledged_output_offset: Some(expected.output_offset),
        last_acknowledged_grid_sequence: None,
    });
    write_message(&mut stream, &hello, deadline)?;
    let mut codec = RemoteCodec::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut requested = false;
    'connection: loop {
        let remaining = remaining(deadline)?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                for message in codec.feed(&buffer[..count]).map_err(io::Error::other)? {
                    match message {
                        RemoteMessage::HelloAck(ack) if !requested => {
                            validate_ack(&expected, &ack)?;
                            write_message(
                                &mut stream,
                                &RemoteMessage::StopSession(StopSession {
                                    controller_epoch: ack.controller_epoch,
                                }),
                                deadline,
                            )?;
                            requested = true;
                        }
                        RemoteMessage::Error(error) if error.code == "session_stopping" => {
                            break 'connection;
                        }
                        RemoteMessage::Error(_) if !requested => {
                            return Err(Failure::StopUnsupported.into_io_error());
                        }
                        RemoteMessage::Error(error) if error.fatal => break 'connection,
                        _ => {}
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(_) if requested => break,
            Err(error) => return Err(error),
        }
    }
    // Socket EOF alone is not an exit fact. Confirm both the authenticated
    // recorded exit and the exact incarnation's released ownership lock.
    wait_stopped(&paths, selector, &expected, deadline)
}

fn validate_ack(expected: &SessionState, ack: &HelloAck) -> io::Result<()> {
    ack.validate()
        .map_err(|_| Failure::StopIdentityMismatch.into_io_error())?;
    if ack.protocol.major != ProtocolVersion::CURRENT.major
        || ack.protocol.minor < STOP_SESSION_PROTOCOL_MINOR
        || !ack.capabilities.contains(&RemoteCapability::StopSession)
        || !ack
            .capabilities
            .contains(&RemoteCapability::ProcessIdentity)
    {
        return Err(Failure::StopUnsupported.into_io_error());
    }
    if ack.holder_build_id != expected.holder_build_id
        || ack.session_incarnation != expected.session_incarnation
        || ack.controller_epoch <= expected.controller_epoch
        || ack.child_identity != expected.child_identity
        || matches!((&expected.process_state, &ack.process_state),
            (RemoteProcessState::Running { pid: first }, RemoteProcessState::Running { pid: second }) if first != second)
        || (matches!(expected.process_state, RemoteProcessState::Exited { .. })
            && expected.process_state != ack.process_state)
        || matches!(
            (&expected.process_state, &ack.process_state),
            (
                RemoteProcessState::Exited { .. },
                RemoteProcessState::Running { .. }
            )
        )
    {
        return Err(Failure::StopIdentityMismatch.into_io_error());
    }
    Ok(())
}

fn wait_stopped(
    paths: &SessionPaths,
    selector: &SessionSelector,
    expected: &SessionState,
    deadline: Instant,
) -> io::Result<SessionInspection> {
    loop {
        let current = read_authenticated(paths, selector)?;
        validate_binding(expected, &current)?;
        if !state::holder_lock_held(&paths.lock)? {
            if !matches!(current.process_state, RemoteProcessState::Exited { .. }) {
                return Err(Failure::HolderUnavailable.into_io_error());
            }
            let _lock = lock_until(paths, deadline)?;
            let current = read_authenticated(paths, selector)?;
            validate_binding(expected, &current)?;
            if state::holder_lock_held(&paths.lock)?
                || !matches!(current.process_state, RemoteProcessState::Exited { .. })
            {
                return Err(Failure::StopIdentityMismatch.into_io_error());
            }
            if current.persistence == diri_proto::remote_pty::PersistenceCapability::UserSupervisor
            {
                crate::persistence::cleanup_holder_until(&current.session_id, deadline)
                    .map_err(|_| Failure::StopPending.into_io_error())?;
            }
            remaining(deadline)?;
            return Ok(current.inspection());
        }
        remaining(deadline)?;
        pause_until(deadline);
    }
}

fn validate_binding(expected: &SessionState, current: &SessionState) -> io::Result<()> {
    if expected.session_id != current.session_id
        || expected.session_incarnation != current.session_incarnation
        || expected.holder_build_id != current.holder_build_id
        || expected.holder_pid != current.holder_pid
        || expected.child_identity != current.child_identity
    {
        return Err(Failure::StopIdentityMismatch.into_io_error());
    }
    Ok(())
}

fn read_authenticated(
    paths: &SessionPaths,
    selector: &SessionSelector,
) -> io::Result<SessionState> {
    if !state::authenticate(paths, &selector.session_token)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session authentication failed",
        ));
    }
    let current = state::read_state(&paths.state)?;
    crate::validate_incarnation(selector, &current)?;
    Ok(current)
}

/// The management deadline includes socket admission. A blocking connect can
/// otherwise wait indefinitely behind a full local listen backlog.
fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    remaining(deadline)?;
    // SAFETY: all-zero sockaddr_un is valid storage before initializing fields.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Holder socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: socket takes only integer constants and returns an owned fd.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful socket call returned a new owned descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: owned holds a live fd; these commands take integer flag values.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(owned);
    stream.set_nonblocking(true)?;
    loop {
        remaining(deadline)?;
        // SAFETY: address is initialized, with a valid bounded length, and
        // stream owns raw throughout this synchronous connect attempt.
        if unsafe { libc::connect(raw, (&raw const address).cast(), length) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                pause_until(deadline);
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                let mut descriptor = libc::pollfd {
                    fd: raw,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let timeout = remaining(deadline)?
                    .as_millis()
                    .max(1)
                    .min(i32::MAX as u128) as i32;
                // SAFETY: descriptor is live initialized storage for one entry.
                let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                remaining(deadline)?;
                if ready == 0 {
                    continue;
                }
                if let Some(error) = stream.take_error()? {
                    return Err(error);
                }
                if descriptor.revents & libc::POLLOUT != 0 {
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Holder socket closed during connect",
                ));
            }
            _ => return Err(error),
        }
    }
    remaining(deadline)?;
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn write_message(
    stream: &mut UnixStream,
    message: &RemoteMessage,
    deadline: Instant,
) -> io::Result<()> {
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    stream.write_all(&RemoteCodec::encode(message).map_err(io::Error::other)?)
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| Failure::StopPending.into_io_error())
}
fn pause_until(deadline: Instant) {
    std::thread::sleep(
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(10)),
    );
}
fn lock_until(paths: &SessionPaths, deadline: Instant) -> io::Result<File> {
    let file = open_private_file(&paths.launch_lock)?;
    loop {
        remaining(deadline)?;
        // SAFETY: a live owned descriptor and ordinary nonblocking flock flags.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                pause_until(deadline)
            }
            _ => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::process::{BootId, ProcessBirth, ProcessIdentity};
    use diri_proto::remote_pty::{ANNOTATED_HOLDER_CAPABILITIES, SessionToken};

    fn fixture() -> (SessionState, SessionSelector) {
        let identity = ProcessIdentity::new(
            42,
            ProcessBirth::Linux {
                boot_id: BootId::parse("12345678-1234-5678-9abc-def012345678").unwrap(),
                start_ticks: 100,
                clock_ticks_per_second: 100,
            },
        )
        .unwrap();
        let state = serde_json::from_value(serde_json::json!({
            "schema":1,"sessionId":"stop-fixture","sessionIncarnation":"incarnation",
            "holderBuildId":"build","holderPid":1,"processState":{"state":"running","pid":42},
            "childIdentity":identity,"cols":80,"rows":24,"outputOffset":0,"snapshotSequence":1,
            "controllerEpoch":1,"persistence":"non-persistent","createdAtUnixMs":0
        }))
        .unwrap();
        let selector = SessionSelector {
            session_id: "stop-fixture".into(),
            session_token: SessionToken::new("0123456789abcdef0123456789abcdef").unwrap(),
            expected_incarnation: Some("incarnation".into()),
        };
        (state, selector)
    }
    fn ack(state: &SessionState) -> HelloAck {
        HelloAck {
            protocol: ProtocolVersion::CURRENT,
            holder_build_id: state.holder_build_id.clone(),
            session_incarnation: state.session_incarnation.clone(),
            capabilities: ANNOTATED_HOLDER_CAPABILITIES.to_vec(),
            controller_epoch: state.controller_epoch + 1,
            process_state: state.process_state.clone(),
            child_identity: state.child_identity,
            output_offset: 0,
            snapshot_sequence: 1,
            foreground_pid: None,
        }
    }
    #[test]
    fn stop_connect_deadline_bounds_an_unaccepted_socket_backlog() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("holder.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // SAFETY: listener owns a live socket; one requests a bounded listen backlog.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let error = connect_until(&socket, Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let mut queued = Vec::new();
        for _ in 0..64 {
            let started = Instant::now();
            match connect_until(&socket, started + Duration::from_millis(25)) {
                Ok(stream) => queued.push(stream),
                Err(error) => {
                    // Linux waits for backlog capacity; macOS refuses it.
                    assert!(matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::ConnectionRefused
                    ));
                    assert!(started.elapsed() < Duration::from_millis(500));
                    return;
                }
            }
        }
        panic!("fixture did not fill the unaccepted listen backlog");
    }

    #[test]
    fn stop_handshake_rejects_old_capabilities_recycled_birth_and_stale_control() {
        let (state, _) = fixture();
        assert!(validate_ack(&state, &ack(&state)).is_ok());
        for change in 0..7 {
            let mut ack = ack(&state);
            match change {
                0 => ack.protocol.minor = 11,
                1 => ack
                    .capabilities
                    .retain(|capability| *capability != RemoteCapability::StopSession),
                2 => ack.controller_epoch = state.controller_epoch,
                3 => ack.holder_build_id.push_str("-changed"),
                4 => ack.session_incarnation.push_str("-changed"),
                5 => ack.child_identity = None,
                _ => {
                    ack.child_identity = Some(
                        ProcessIdentity::new(
                            42,
                            ProcessBirth::Linux {
                                boot_id: BootId::parse("12345678-1234-5678-9abc-def012345678")
                                    .unwrap(),
                                start_ticks: 101,
                                clock_ticks_per_second: 100,
                            },
                        )
                        .unwrap(),
                    )
                }
            }
            assert!(validate_ack(&state, &ack).is_err());
        }
        let mut exited = ack(&state);
        exited.process_state = RemoteProcessState::Exited {
            code: Some(42),
            signal: None,
        };
        assert!(
            validate_ack(&state, &exited).is_ok(),
            "actual exit during handshake remains identifiable"
        );
        let mut replacement = state.clone();
        replacement.session_incarnation.push_str("-new");
        assert!(validate_binding(&state, &replacement).is_err());
    }
    #[test]
    fn stop_wait_never_turns_missing_owner_or_deadline_into_exit() {
        let temporary = tempfile::tempdir().unwrap();
        let roots = StatePaths::from_root(temporary.path().join("state")).unwrap();
        let (mut state, selector) = fixture();
        let paths = roots.session(&state.session_id).unwrap();
        paths.ensure().unwrap();
        state::initialize_auth(&paths, &selector.session_token).unwrap();
        state::write_state(&paths.state, &state).unwrap();
        let expected = state.clone();
        assert_eq!(
            wait_stopped(
                &paths,
                &selector,
                &expected,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::NotConnected
        );
        let lock = state::acquire_lock(&paths.lock).unwrap();
        assert_eq!(
            wait_stopped(&paths, &selector, &expected, Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            state::read_state(&paths.state).unwrap().process_state,
            expected.process_state
        );
        state.process_state = RemoteProcessState::Exited {
            code: Some(42),
            signal: None,
        };
        state::write_state(&paths.state, &state).unwrap();
        assert_eq!(
            wait_stopped(&paths, &selector, &expected, Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        drop(lock);
        assert_eq!(
            wait_stopped(
                &paths,
                &selector,
                &expected,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap()
            .process_state,
            state.process_state
        );
        state.session_incarnation.push_str("-new");
        state::write_state(&paths.state, &state).unwrap();
        assert!(
            wait_stopped(
                &paths,
                &selector,
                &expected,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
