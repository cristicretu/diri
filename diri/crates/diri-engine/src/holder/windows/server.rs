//! One native Windows Holder, one ConPTY/Job, one owner loop. Request workers
//! only frame bounded messages; they never own the PTY, log or terminal state.
use super::{HolderError, HolderResult, fanout::FrameQueue, protocol::*, socket};
use crate::{Exit, OutputLog, Pty, PtySpec};
use base64::Engine as _;
use diri_platform::{
    ipc::{UnixListener, UnixStream},
    poll::{self, AsRawIo},
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

enum Request {
    Control(HolderRequest, mpsc::SyncSender<HolderResponse>),
    Subscribe(Arc<FrameQueue>, mpsc::SyncSender<HolderResponse>),
}
struct PendingInput {
    bytes: Vec<u8>,
    written: usize,
    reply: mpsc::SyncSender<HolderResponse>,
}

pub struct HolderServer;
impl HolderServer {
    pub fn run(spec: HolderLaunchSpec) -> HolderResult<()> {
        run(spec).map_err(|e| HolderError::io("Windows Holder", e))
    }
}
fn run(spec: HolderLaunchSpec) -> io::Result<()> {
    let parent = Path::new(&spec.socket_path)
        .parent()
        .ok_or_else(|| io::Error::other("Holder directory missing"))?;
    diri_platform::security::private_dir_all(parent)?;
    let lock_path = Path::new(&spec.pid_file_path).with_extension("owner.lock");
    let owner = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    owner.try_lock().map_err(io::Error::other)?;
    let listener = UnixListener::bind(&spec.socket_path)?;
    let log_path = Path::new(&spec.log_file_path);
    let log_dir = log_path
        .parent()
        .ok_or_else(|| io::Error::other("Holder log directory missing"))?;
    diri_platform::security::private_dir_all(log_dir)?;
    let mut log = OutputLog::open(
        log_dir,
        log_path
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or(&spec.session_id),
        0,
        if spec.disk_capacity > 0 {
            spec.disk_capacity as usize
        } else {
            DEFAULT_DISK_CAPACITY as usize
        },
        false,
    )?;
    let epoch = log.tail_offset();
    let launch = PtySpec {
        argv: spec.argv.clone(),
        env: spec
            .environment
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        cwd: spec.cwd.clone().into(),
        cols: spec.cols,
        rows: spec.rows,
    };
    let mut pty = Pty::spawn(&launch)?;
    let mut output = pty.reader()?;
    let mut input = pty.writer()?;
    output.set_nonblocking(true)?;
    input.set_nonblocking(true)?;
    std::fs::write(&spec.pid_file_path, std::process::id().to_string())?;
    let (mut wake_read, wake_write) = UnixStream::pair()?;
    wake_read.set_nonblocking(true)?;
    wake_write.set_nonblocking(true)?;
    let mut exit_wake = wake_write.try_clone()?;
    let watcher = diri_pty::ExitWatcher::new(pty.pid())?;
    std::thread::Builder::new()
        .name("holder-process-exit".into())
        .spawn(move || {
            let _ = watcher.wait(None);
            let _ = exit_wake.write(&[1]);
        })?;
    let (requests, incoming) = mpsc::sync_channel(32);
    let workers = Arc::new(AtomicUsize::new(0));
    let active_workers = Arc::clone(&workers);
    std::thread::Builder::new()
        .name("holder-accept".into())
        .spawn(move || {
            for client in listener.incoming().flatten() {
                if active_workers
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        (n < 32).then_some(n + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let count = Arc::clone(&active_workers);
                let sender = requests.clone();
                let Ok(wake) = wake_write.try_clone() else {
                    count.fetch_sub(1, Ordering::AcqRel);
                    continue;
                };
                let error_count = Arc::clone(&count);
                if std::thread::Builder::new()
                    .name("holder-client".into())
                    .spawn(move || {
                        struct Active(Arc<AtomicUsize>);
                        impl Drop for Active {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                        let _active = Active(count);
                        let _ = serve(client, sender, wake);
                    })
                    .is_err()
                {
                    error_count.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })?;
    let mut subscribers: Vec<Arc<FrameQueue>> = Vec::new();
    let mut pending: VecDeque<PendingInput> = VecDeque::new();
    let mut pending_bytes = 0usize;
    let mut exit = None;
    let mut eof = false;
    let mut buffer = [0u8; 65536];
    loop {
        if exit.is_none()
            && let Some(status) = pty.try_wait()?
        {
            exit = Some(status);
            pty.kill_group(9)?; // descendants cannot hold the console open forever
            pty.begin_close()?;
            for request in pending.drain(..) {
                let _ = request.reply.send(HolderResponse::failure(
                    "Agent exited before queued input completed",
                ));
            }
            pending_bytes = 0;
        }
        while let Ok(request) = incoming.try_recv() {
            match request {
                Request::Subscribe(queue, reply) => {
                    if exit.is_some() {
                        let _ = reply.send(HolderResponse::failure("Agent exited"));
                        queue.close();
                    } else {
                        let _ = reply.send(HolderResponse::output_stream(
                            HOLDER_OUTPUT_STREAM_VERSION,
                            log.tail_offset(),
                        ));
                        subscribers.push(queue);
                    }
                }
                Request::Control(request, reply) => {
                    let result = match request.op {
                        HolderOperation::Stat => Ok(HolderResponse::with_stat(HolderStat {
                            child_identity: pty.child_identity().filter(|identity| {
                                exit.is_none()
                                    && diri_pty::process_identity::observe(identity.pid())
                                        .ok()
                                        .as_ref()
                                        == Some(identity)
                            }),
                            child_pid: pty.pid() as i32,
                            alive: exit.is_none(),
                            log_offset: log.tail_offset(),
                            foreground_pid: None,
                            cols: Some(pty.size()?.0),
                            rows: Some(pty.size()?.1),
                            epoch_offset: Some(epoch),
                            secret_input: None,
                            awaiting_line: None,
                        })),
                        _ if exit.is_some() => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                        HolderOperation::Write => {
                            let bytes = request
                                .data
                                .as_deref()
                                .ok_or_else(|| io::Error::other("missing input"))
                                .and_then(|v| {
                                    base64::engine::general_purpose::STANDARD
                                        .decode(v)
                                        .map_err(io::Error::other)
                                });
                            match bytes {
                                Ok(bytes)
                                    if bytes.len() <= HOLDER_STREAM_MAX_PAYLOAD
                                        && pending_bytes + bytes.len()
                                            <= 2 * HOLDER_STREAM_MAX_PAYLOAD =>
                                {
                                    pending_bytes += bytes.len();
                                    pending.push_back(PendingInput {
                                        bytes,
                                        written: 0,
                                        reply,
                                    });
                                    continue;
                                }
                                Ok(_) => Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "Holder input queue full",
                                )),
                                Err(e) => Err(e),
                            }
                        }
                        HolderOperation::Resize => match (request.cols, request.rows) {
                            (Some(cols), Some(rows)) => {
                                pty.resize(cols, rows).map(|()| HolderResponse::success())
                            }
                            _ => Err(io::Error::other("missing dimensions")),
                        },
                        HolderOperation::Signal => {
                            pty.kill_group(request.sig.unwrap_or(0)).map(|()| {
                                HolderResponse::with_tree(super::process_tree::enumerate(
                                    pty.pid() as i32
                                ))
                            })
                        }
                        HolderOperation::KillTree => {
                            pty.kill_group(9).map(|()| HolderResponse::success())
                        }
                        _ => Err(io::Error::other("invalid Holder operation")),
                    };
                    let _ = reply
                        .send(result.unwrap_or_else(|e| HolderResponse::failure(e.to_string())));
                }
            }
        }
        let mut budget = 65536;
        while let Some(request) = pending.front_mut() {
            let remaining = &request.bytes[request.written..];
            if remaining.is_empty() {
                let request = pending.pop_front().expect("front");
                pending_bytes -= request.bytes.len();
                let _ = request.reply.send(HolderResponse::success());
                continue;
            }
            if budget == 0 {
                break;
            }
            match input.write(&remaining[..remaining.len().min(budget)]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    request.written += n;
                    budget -= n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let start = Instant::now();
        let mut drained = 0;
        while !eof && drained < 65536 && start.elapsed() < Duration::from_millis(2) {
            match output.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    publish(&mut log, &mut subscribers, &buffer[..n])?;
                    drained += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if eof && let Some(status) = exit {
            let Exit::Code(code) = status else {
                return Err(io::Error::other("Windows reported a POSIX signal exit"));
            };
            let marker = HolderExitMarker::encode(&HolderExitStatus {
                reason: HolderExitReason::Exited,
                code: Some(code),
                signal: None,
            });
            publish(&mut log, &mut subscribers, &marker)?;
            log.flush()?;
            break;
        }
        let mut entries = [
            poll::PollFd {
                fd: if eof { -1 } else { output.as_raw_io() },
                events: poll::POLLIN,
                revents: 0,
            },
            poll::PollFd {
                fd: wake_read.as_raw_io(),
                events: poll::POLLIN,
                revents: 0,
            },
            poll::PollFd {
                fd: if pending.is_empty() {
                    -1
                } else {
                    input.as_raw_io()
                },
                events: poll::POLLOUT,
                revents: 0,
            },
        ];
        // SAFETY: the three owning objects outlive this wait. Exit and requests
        // wake the same loop; an idle Holder has no timer or polling deadline.
        if unsafe { poll::poll(entries.as_mut_ptr(), entries.len(), -1) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut wake = [0u8; 256];
        while wake_read.read(&mut wake).is_ok_and(|n| n > 0) {}
    }
    for subscriber in &subscribers {
        subscriber.close();
    }
    let _ = std::fs::remove_file(&spec.pid_file_path);
    // Final bytes are durable even if the Engine disconnects before draining its
    // output subscription. Its existing offset/log recovery handles that case.
    let deadline = Instant::now() + Duration::from_millis(250);
    while workers.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn publish(
    log: &mut OutputLog,
    subscribers: &mut Vec<Arc<FrameQueue>>,
    bytes: &[u8],
) -> io::Result<()> {
    let offset = log.tail_offset();
    log.append(bytes)?;
    subscribers.retain(|queue| {
        if queue.push(offset, bytes, Duration::ZERO) {
            true
        } else {
            queue.close();
            false
        }
    });
    Ok(())
}
fn call(
    sender: &mpsc::SyncSender<Request>,
    wake: &mut UnixStream,
    request: HolderRequest,
) -> io::Result<HolderResponse> {
    let (reply, response) = mpsc::sync_channel(1);
    sender
        .try_send(Request::Control(request, reply))
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "Holder request queue full"))?;
    match wake.write(&[1]) {
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
        Err(e) => return Err(e),
    }
    response
        .recv_timeout(Duration::from_secs(10))
        .map_err(io::Error::other)
}
fn serve(
    mut stream: UnixStream,
    sender: mpsc::SyncSender<Request>,
    mut wake: UnixStream,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request: HolderRequest = socket::read_json_line(&mut stream).map_err(io::Error::other)?;
    match request.op {
        HolderOperation::OutputStream
            if request.stream_version == Some(HOLDER_OUTPUT_STREAM_VERSION) =>
        {
            let queue = FrameQueue::new(1 << 20);
            let (reply, response) = mpsc::sync_channel(1);
            sender
                .try_send(Request::Subscribe(Arc::clone(&queue), reply))
                .map_err(io::Error::other)?;
            let _ = wake.write(&[1]);
            let response = response
                .recv_timeout(Duration::from_secs(10))
                .map_err(io::Error::other)?;
            socket::write_json_line(&mut stream, &response).map_err(io::Error::other)?;
            if !response.ok {
                return Ok(());
            }
            while let Some((offset, bytes)) = queue.pop() {
                if stream
                    .write_all(&offset.to_be_bytes())
                    .and_then(|()| stream.write_all(&(bytes.len() as u32).to_be_bytes()))
                    .and_then(|()| stream.write_all(&bytes))
                    .is_err()
                {
                    break;
                }
            }
            queue.close();
        }
        HolderOperation::Stream if request.stream_version == Some(HOLDER_STREAM_VERSION) => {
            let mut response = HolderResponse::success();
            response.stream_version = Some(HOLDER_STREAM_VERSION);
            socket::write_json_line(&mut stream, &response).map_err(io::Error::other)?;
            stream.set_read_timeout(None)?;
            loop {
                let mut header = [0u8; 5];
                stream.read_exact(&mut header)?;
                let length = u32::from_be_bytes(header[1..].try_into().expect("header")) as usize;
                if length > HOLDER_STREAM_MAX_PAYLOAD {
                    return Err(io::Error::other("oversized Holder input"));
                }
                let mut bytes = vec![0u8; length];
                stream.read_exact(&mut bytes)?;
                let request = match header[0] {
                    HOLDER_STREAM_INPUT => {
                        let mut request = HolderRequest::op(HolderOperation::Write);
                        request.data =
                            Some(base64::engine::general_purpose::STANDARD.encode(bytes));
                        request
                    }
                    HOLDER_STREAM_RESIZE if bytes.len() == 4 => {
                        let mut request = HolderRequest::op(HolderOperation::Resize);
                        request.cols = Some(u16::from_be_bytes([bytes[0], bytes[1]]));
                        request.rows = Some(u16::from_be_bytes([bytes[2], bytes[3]]));
                        request
                    }
                    _ => return Err(io::Error::other("invalid Holder input frame")),
                };
                let response = call(&sender, &mut wake, request)?;
                stream.write_all(&[if response.ok { HOLDER_STREAM_ACK } else { 1 }])?;
                if !response.ok {
                    return Ok(());
                }
            }
        }
        _ => {
            let response = call(&sender, &mut wake, request)?;
            socket::write_json_line(&mut stream, &response).map_err(io::Error::other)?;
        }
    }
    Ok(())
}
