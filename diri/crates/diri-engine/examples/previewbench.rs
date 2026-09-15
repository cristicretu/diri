//! Disposable Engine-local preview load. No GUI, SSH, or user session is used.
//! `previewbench COUNT FPS SECONDS` alternates 80x24 / 160x50 terminal grids.
//! The candidate count must fit the explicitly recorded compiled preview cap.
use diri_engine::attach::AttachHub;
use diri_engine::registry::Registry;
use diri_engine::session::SessionSpec;
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::SessionId;
use diri_proto::frames::{Frame, FrameCodec, FrameType};
use diri_proto::preview_set::*;
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
fn producer(args: &[String]) {
    let cols: usize = args[2].parse().unwrap();
    let rows: usize = args[3].parse().unwrap();
    let fps: u64 = args[4].parse().unwrap();
    // Readiness is emitted only after kernel echo is off, so echo cannot fake
    // a fast input response before the child processed it.
    unsafe {
        let mut term = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(0, &mut term), 0);
        term.c_lflag &= !libc::ECHO;
        assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &term), 0);
    }
    print!("\x1b[?2004hready");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    let mut input = std::io::stdin().lock();
    if args[1] == "--echo" {
        while input.read_line(&mut line).unwrap() > 0 {
            print!("\x1b[H{}", line.trim());
            std::io::stdout().flush().unwrap();
            line.clear();
        }
        return;
    }
    input.read_line(&mut line).unwrap();
    if fps == 0 {
        loop {
            std::thread::park();
        }
    }
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut sequence = 0u64;
    let mut diagnostics = args
        .get(5)
        .filter(|path| !path.is_empty())
        .map(|path| std::fs::File::create(path).unwrap());
    loop {
        let stamp = now_ns();
        let mut frame = String::with_capacity(cols * rows + rows * 20);
        for row in 0..rows {
            frame.push_str(&format!(
                "\x1b[{};1H\x1b[38;5;{}m",
                row + 1,
                20 + sequence % 100
            ));
            let text = if row == 0 {
                format!("T{stamp:020} ")
            } else {
                String::new()
            };
            frame.push_str(&text);
            for column in text.len()..cols {
                if column % 12 == 0 {
                    frame.push_str(&format!(
                        "\x1b[38;5;{}m",
                        20 + (sequence as usize + row + column / 12) % 100
                    ));
                }
                frame.push(char::from(
                    b'!' + ((row * cols + column + sequence as usize) % 90) as u8,
                ));
            }
        }
        let write_started = Instant::now();
        if std::io::stdout().write_all(frame.as_bytes()).is_err() {
            return;
        }
        std::io::stdout().flush().unwrap();
        if let Some(diagnostics) = &mut diagnostics {
            writeln!(
                diagnostics,
                "{} {} {}",
                stamp,
                write_started.elapsed().as_micros(),
                frame.len()
            )
            .unwrap();
        }
        sequence += 1;
        next += period;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        } else {
            next = Instant::now();
        }
    }
}

struct Reader {
    stream: BufReader<UnixStream>,
    codec: FrameCodec,
    queued: VecDeque<Frame>,
    bytes: u64,
    dimensions: (u16, u16),
    eof: bool,
}
impl Reader {
    fn next(&mut self, deadline: Instant) -> Option<Frame> {
        loop {
            if let Some(frame) = self.queued.pop_front() {
                return Some(frame);
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut bytes = [0u8; 64 * 1024];
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    self.eof = true;
                    return None;
                }
                Ok(count) => {
                    self.bytes += count as u64;
                    self.queued
                        .extend(self.codec.feed(&bytes[..count]).unwrap());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => return None,
            }
        }
    }
}
struct MuxReader {
    stream: BufReader<UnixStream>,
    decoder: PreviewSetDecoder,
    queued: VecDeque<PreviewSetPacket>,
    dimensions: BTreeMap<String, (u16, u16)>,
    eof: bool,
}
impl MuxReader {
    fn next(&mut self, deadline: Instant) -> Option<PreviewSetPacket> {
        loop {
            if let Some(packet) = self.queued.pop_front() {
                return Some(packet);
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut bytes = [0; 64 * 1024];
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    self.eof = true;
                    return None;
                }
                Ok(count) => self
                    .queued
                    .extend(self.decoder.feed(&bytes[..count]).unwrap()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("mux read: {error}"),
            }
        }
    }
}
fn open_mux(
    hub: &AttachHub,
    registry: &Arc<Mutex<Registry>>,
    ids: &[String],
) -> (MuxReader, std::thread::JoinHandle<()>) {
    let (server, client) = UnixStream::pair().unwrap();
    let hub = hub.clone();
    let registry = Arc::clone(registry);
    let worker = std::thread::spawn(move || {
        let _ = hub.serve_preview_set(&registry, server, Vec::new());
    });
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut stream = BufReader::new(client);
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    let ready: PreviewSetReady = serde_json::from_str(&line).unwrap();
    assert_eq!(ready.version, PREVIEW_SET_VERSION);
    let membership = PreviewSetMembership {
        members: ids
            .iter()
            .map(|id| PreviewMember {
                session_id: SessionId::new(id),
                generation: 1,
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec(&membership).unwrap();
    bytes.push(b'\n');
    stream.get_mut().write_all(&bytes).unwrap();
    let mut reader = MuxReader {
        stream,
        decoder: PreviewSetDecoder::default(),
        queued: VecDeque::new(),
        dimensions: BTreeMap::new(),
        eof: false,
    };
    let mut modes = std::collections::HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while reader.dimensions.len() != ids.len() || modes.len() != ids.len() {
        match reader.next(deadline).expect("mux initial seed") {
            PreviewSetPacket::Unavailable { member, reason } => {
                panic!("mux unavailable: {member:?} {reason:?}")
            }
            PreviewSetPacket::Chunk { member, frame } => {
                if let Some(grid) = frame.grid_payload().unwrap() {
                    if !reader.dimensions.contains_key(&member.session_id.0) {
                        assert!(grid.is_full_snapshot);
                    }
                    reader
                        .dimensions
                        .insert(member.session_id.0, (grid.cols, grid.rows));
                } else if frame.frame_type == FrameType::Modes {
                    modes.insert(member.session_id.0);
                }
            }
        }
    }
    (reader, worker)
}
#[derive(Default)]
struct ReaderSamples {
    dimensions: (u16, u16),
    latency: Vec<u64>,
    stamp: u128,
    frames: u64,
    bytes: u64,
    eof: bool,
    lifetime: u64,
}
impl ReaderSamples {
    fn observe(&mut self, frame: &Frame) {
        if let Some(grid) = frame.grid_payload().unwrap() {
            self.frames += 1;
            if let Some(row) = grid.changed_rows.iter().find(|row| row.y == 0) {
                let text: String = row
                    .cells
                    .iter()
                    .take(21)
                    .filter_map(|cell| char::from_u32(cell.scalar))
                    .collect();
                if let Some(parsed) = text.strip_prefix('T').and_then(|s| s.parse::<u128>().ok())
                    && parsed != self.stamp
                {
                    self.stamp = parsed;
                    self.latency
                        .push((now_ns().saturating_sub(parsed) / 1000) as u64);
                }
            }
        }
    }
}
fn open(
    hub: &AttachHub,
    registry: &Arc<Mutex<Registry>>,
    id: &str,
    preview: bool,
    slow: bool,
) -> (Reader, std::thread::JoinHandle<()>) {
    let (server, client) = UnixStream::pair().unwrap();
    if slow {
        let size: libc::c_int = 1024;
        unsafe {
            assert_eq!(
                libc::setsockopt(
                    server.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as _
                ),
                0
            );
        }
    }
    let writer = Arc::new(Mutex::new(server.try_clone().unwrap()));
    let hub = hub.clone();
    let registry = Arc::clone(registry);
    let id = id.to_owned();
    let thread = std::thread::spawn(move || {
        if preview {
            hub.serve_preview(&registry, &id, server, Vec::new(), writer);
        } else {
            hub.serve(&registry, &id, server, Vec::new(), writer);
        }
    });
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut stream = BufReader::new(client);
    if preview {
        let mut line = String::new();
        stream.read_line(&mut line).expect("preview admission");
        let _: diri_proto::preview::PreviewReady =
            serde_json::from_str(&line).expect("admitted preview");
    }
    let mut reader = Reader {
        stream,
        codec: FrameCodec::new(),
        queued: VecDeque::new(),
        bytes: 0,
        dimensions: (0, 0),
        eof: false,
    };
    let seed = reader
        .next(Instant::now() + Duration::from_secs(5))
        .unwrap()
        .grid_payload()
        .unwrap()
        .unwrap();
    assert!(seed.is_full_snapshot);
    reader.dimensions = (seed.cols, seed.rows);
    assert_eq!(
        reader
            .next(Instant::now() + Duration::from_secs(5))
            .unwrap()
            .frame_type,
        FrameType::Modes
    );
    (reader, thread)
}
fn cpu_seconds() -> f64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage.ru_utime.tv_sec as f64
            + usage.ru_utime.tv_usec as f64 / 1e6
            + usage.ru_stime.tv_sec as f64
            + usage.ru_stime.tv_usec as f64 / 1e6
    }
}
fn rss_kib() -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn percentile(samples: &[u64], percent: usize) -> Option<u64> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted
        .get(sorted.len().saturating_sub(1) * percent / 100)
        .copied()
}
fn thread_count() -> usize {
    let output = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string(), "-o", "pid="])
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}
fn peak_rss_kib() -> u64 {
    // SAFETY: getrusage writes one owned, initialized rusage value.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        if cfg!(target_os = "macos") {
            usage.ru_maxrss as u64 / 1024
        } else {
            usage.ru_maxrss as u64
        }
    }
}
#[derive(Default)]
struct DimensionSamples {
    readers: usize,
    eof_readers: usize,
    lifetimes_ms: Vec<u64>,
    latency: Vec<u64>,
    frames: u64,
    bytes: u64,
}

struct Cleanup(Arc<Mutex<Registry>>, Vec<String>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in &self.1 {
            let _ = self.0.lock().unwrap().terminate(id, Duration::ZERO);
        }
    }
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a.starts_with("--")) {
        producer(&args);
        return;
    }
    let count: usize = args[1].parse().unwrap();
    let fps: u64 = args[2].parse().unwrap();
    let seconds: u64 = args.get(3).map_or(4, |a| a.parse().unwrap());
    assert!((2..=diri_proto::preview::MAX_PREVIEWS).contains(&count));
    let diagnose = args.iter().any(|arg| arg == "diagnose");
    let multiplex = args.iter().any(|arg| arg == "mux");
    let root = tempfile::tempdir().unwrap();
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).unwrap();
    let engine = Arc::new(engine);
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::clone(&engine),
        root.path().join("state.json"),
    )));
    let ids: Vec<String> = (0..=count)
        .map(|index| format!("preview-{index}"))
        .collect();
    let _cleanup = Cleanup(Arc::clone(&registry), ids.clone());
    for (index, id) in ids.iter().enumerate() {
        let (cols, rows) = if index % 2 == 0 { (80, 24) } else { (160, 50) };
        let mode = if index == count {
            "--echo"
        } else {
            "--producer"
        };
        let record = serde_json::from_value(json!({"id":id,"kind":diri_proto::AgentKind::SHELL,"cwd":root.path(),
            "projectID":"fixture","title":"fixture","titleSource":diri_proto::TitleSource::Placeholder,"status":diri_proto::SessionStatus::Idle,
            "resumability":diri_proto::Resumability::Live,"createdAt":0,"updatedAt":0,"pinned":false})).unwrap();
        registry
            .lock()
            .unwrap()
            .spawn(
                SessionSpec {
                    id: id.clone(),
                    pty: PtySpec::new(
                        vec![
                            std::env::current_exe()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                            mode.into(),
                            cols.to_string(),
                            rows.to_string(),
                            fps.to_string(),
                            if diagnose {
                                root.path()
                                    .join(format!("{id}.progress"))
                                    .to_string_lossy()
                                    .into_owned()
                            } else {
                                String::new()
                            },
                        ],
                        root.path(),
                    )
                    .size(cols, rows),
                    manifest_id: "shell".into(),
                    authority: Authority::ProcessOnly,
                    logs_dir: root.path().join("logs"),
                    holder: None,
                    remote: None,
                    defer_launch: false,
                },
                record,
            )
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ids
            .iter()
            .all(|id| registry.lock().unwrap().get(id).unwrap().bracketed_paste())
        {
            break;
        }
        assert!(Instant::now() < deadline, "producer readiness");
        std::thread::sleep(Duration::from_millis(2));
    }
    let baseline_rss = rss_kib();
    let baseline_threads = thread_count();
    let baseline_cpu = cpu_seconds();
    std::thread::sleep(Duration::from_secs(1));
    let unattached_idle_cpu = cpu_seconds() - baseline_cpu;
    let hub = AttachHub::new();
    let mut servers = Vec::new();
    let mut peers = Vec::new();
    let (mut slow, server) = open(&hub, &registry, &ids[0], true, true);
    servers.push(server);
    let mut mux = None;
    if multiplex {
        let (reader, server) = open_mux(&hub, &registry, &ids[1..count]);
        mux = Some(reader);
        servers.push(server);
    } else {
        for id in &ids[1..count] {
            let (reader, server) = open(&hub, &registry, id, true, false);
            peers.push(reader);
            servers.push(server);
        }
    }
    let (mut active, server) = open(&hub, &registry, &ids[count], false, false);
    servers.push(server);
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut readers: Vec<_> = peers
        .into_iter()
        .map(|mut reader| {
            std::thread::spawn(move || {
                let reader_started = Instant::now();
                let mut samples = ReaderSamples {
                    dimensions: reader.dimensions,
                    ..Default::default()
                };
                while let Some(frame) = reader.next(end) {
                    samples.observe(&frame);
                }
                samples.bytes = reader.bytes;
                samples.eof = reader.eof;
                samples.lifetime = reader_started.elapsed().as_millis() as u64;
                let _ = reader.stream.get_ref().shutdown(std::net::Shutdown::Both);
                vec![samples]
            })
        })
        .collect();
    if let Some(mut reader) = mux {
        readers.push(std::thread::spawn(move || {
            let reader_started = Instant::now();
            let mut samples: BTreeMap<_, _> = reader
                .dimensions
                .iter()
                .map(|(id, dimensions)| {
                    (
                        id.clone(),
                        ReaderSamples {
                            dimensions: *dimensions,
                            ..Default::default()
                        },
                    )
                })
                .collect();
            while let Some(packet) = reader.next(end) {
                match packet {
                    PreviewSetPacket::Chunk { member, frame } => {
                        let sample = samples.get_mut(&member.session_id.0).unwrap();
                        let frame_bytes = frame.payload.len() + 5;
                        sample.bytes += (frame_bytes
                            + PreviewSetHeader::Chunk {
                                member,
                                frame_bytes,
                            }
                            .encode()
                            .unwrap()
                            .len()) as u64;
                        sample.observe(&frame);
                    }
                    PreviewSetPacket::Unavailable { member, reason } => {
                        panic!("mux source became unavailable: {member:?} {reason:?}")
                    }
                }
            }
            let lifetime = reader_started.elapsed().as_millis() as u64;
            for sample in samples.values_mut() {
                sample.eof = reader.eof;
                sample.lifetime = lifetime;
            }
            let _ = reader.stream.get_ref().shutdown(std::net::Shutdown::Both);
            samples.into_values().collect()
        }));
    }
    let attached_rss = rss_kib();
    let attached_threads = thread_count();
    let start = Instant::now();
    let cpu_start = cpu_seconds();
    for id in ids[..count].iter().filter(|_| fps > 0) {
        registry
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .write_input(b"start\n")
            .unwrap();
    }
    let mut input_us = Vec::new();
    for index in 0..if fps > 0 { 101 } else { 0 } {
        let marker = format!("input-{index:03}");
        let at = Instant::now();
        active
            .stream
            .get_mut()
            .write_all(
                &FrameCodec::encode(&Frame::input(format!("{marker}\n").into_bytes())).unwrap(),
            )
            .unwrap();
        loop {
            let frame = active
                .next(Instant::now() + Duration::from_secs(2))
                .expect("active input under load");
            if let Some(grid) = frame.grid_payload().unwrap() {
                let text: String = grid
                    .changed_rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .filter_map(|cell| char::from_u32(cell.scalar))
                    .collect();
                if text.contains(&marker) {
                    input_us.push(at.elapsed().as_micros() as u64);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Instant::now() < end,
        "input samples must complete while previews remain loaded"
    );
    if let Some(wait) = end.checked_duration_since(Instant::now()) {
        std::thread::sleep(wait);
    }
    let cpu = cpu_seconds() - cpu_start;
    let wall = start.elapsed().as_secs_f64();
    let loaded_rss = rss_kib();
    let peak_rss = peak_rss_kib();
    let mut latency = Vec::new();
    let mut frames = 0;
    let mut bytes = 0;
    let mut groups: BTreeMap<(u16, u16), DimensionSamples> = BTreeMap::new();
    for result in readers
        .into_iter()
        .flat_map(|reader| reader.join().unwrap())
    {
        let ReaderSamples {
            dimensions,
            latency: samples,
            frames: f,
            bytes: b,
            eof,
            lifetime,
            ..
        } = result;
        let group = groups.entry(dimensions).or_default();
        group.readers += 1;
        group.eof_readers += usize::from(eof);
        group.lifetimes_ms.push(lifetime);
        group.latency.extend_from_slice(&samples);
        group.frames += f;
        group.bytes += b;
        latency.extend(samples);
        frames += f;
        bytes += b;
    }
    let readers_survived = groups.values().all(|group| group.eof_readers == 0);
    let by_dimensions: Vec<_> = groups.into_iter().map(|((cols, rows), group)| {
        let DimensionSamples { readers, eof_readers, lifetimes_ms, latency: samples, frames, bytes } = group;
        json!({"cols":cols,"rows":rows,"readers":readers,"eof_readers":eof_readers,"reader_lifetimes_ms":lifetimes_ms,"samples":samples.len(),
            "distinct_timestamps_per_second_per_reader":samples.len() as f64 / wall / readers as f64,
            "frames_per_second_per_reader":frames as f64 / wall / readers as f64,
            "output_p50_us":percentile(&samples,50),"output_p90_us":percentile(&samples,90),
            "output_max_us":samples.iter().max(),"bytes":bytes})
    }).collect();
    let diagnostics: Vec<_> = if diagnose {
        ids[..count].iter().map(|id| {
        let progress = std::fs::read_to_string(root.path().join(format!("{id}.progress"))).unwrap_or_default();
        let writes: Vec<u64> = progress.lines().filter_map(|line| line.split_whitespace().nth(1)?.parse().ok()).collect();
        let registry = registry.lock().unwrap();
        let session = registry.get(id).unwrap();
        let tail = session.output_tail();
        let (_, raw) = session.read_output(0, 4 << 20);
        let timestamps: Vec<u128> = raw.windows(21).filter(|word| word[0] == b'T' && word[1..].iter().all(u8::is_ascii_digit)).map(|word| std::str::from_utf8(&word[1..]).unwrap().parse().unwrap()).collect();
        json!({"session":id,"producer_completed_writes":writes.len(),"write_p50_us":percentile(&writes,50),"write_p90_us":percentile(&writes,90),"write_max_us":writes.iter().max(),
            "pty_logged_bytes":tail,"pty_logged_timestamps":timestamps.len(),"engine_first_row":session.screen_lines().first().map(|line| line.chars().take(21).collect::<String>())})
    }).collect()
    } else {
        Vec::new()
    };
    // Drain the slow peer only after the load period. EOF proves bounded writer
    // recovery; no continuation may splice stale/partial frames after overflow.
    let slow_closed = if fps > 0 {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut scratch = [0u8; 64 * 1024];
        let mut closed = false;
        while Instant::now() < deadline {
            match slow.stream.read(&mut scratch) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(closed, "slow peer must disconnect");
        closed
    } else {
        false
    };
    let _ = slow.stream.get_ref().shutdown(std::net::Shutdown::Both);
    let _ = active.stream.get_ref().shutdown(std::net::Shutdown::Both);
    for server in servers.drain(..) {
        server.join().unwrap();
    }
    let pid_before = registry.lock().unwrap().get(&ids[0]).unwrap().child_pid();
    let (reseed, server) = open(&hub, &registry, &ids[0], true, false);
    servers.push(server);
    assert_eq!(
        registry.lock().unwrap().get(&ids[0]).unwrap().child_pid(),
        pid_before
    );
    let _ = reseed.stream.get_ref().shutdown(std::net::Shutdown::Both);
    let _ = active.stream.get_ref().shutdown(std::net::Shutdown::Both);
    for server in servers {
        server.join().unwrap();
    }
    println!(
        "{}",
        json!({"mode": if multiplex { "multiplex" } else { "separate" }, "stalled_preview_transport":"separate single-session socket in both modes", "count":count,"compiled_cap":diri_proto::preview::MAX_PREVIEWS,"fps":fps,"seconds":wall,
        "dimensions":"alternating 80x24 and 160x50","unattached_idle_cpu_seconds_per_second":unattached_idle_cpu,
        "rss_baseline_kib":baseline_rss,"baseline_threads":baseline_threads,"attached_threads":attached_threads,"peak_rss_kib":peak_rss,"normal_active_attachments":1,"stalled_previews":1,"rss_attached_kib":attached_rss,"rss_loaded_kib":loaded_rss,
        "process_cpu_seconds":cpu,"process_cpu_cores":cpu/wall,"input_p50_us":percentile(&input_us,50),
        "input_p95_us":percentile(&input_us,95),"input_max_us":input_us.iter().max(),"input_samples":input_us,
        "readers_survived":readers_survived,"diagnostics":diagnostics,"by_dimensions":by_dimensions,"output_samples":latency.len(),"output_p50_us":percentile(&latency,50),"output_p90_us":percentile(&latency,90),
        "output_max_us":latency.iter().max(),"frames":frames,"bytes":bytes,"slow_peer_disconnected":slow_closed,
        "reseed_same_pid":true,"boundary":"combined Engine and decoded client threads; excludes GUI, SSH and producer CPU"})
    );
    assert!(
        readers_survived,
        "a continuously drained preview disconnected under load"
    );
}
