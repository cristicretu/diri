//! How fast one Holder drains a PTY, with no Engine behind it: a held child
//! `cat`s a file, and the clock stops when `cat` returns, which is when the
//! Holder has read its last byte from the kernel.
//!
//! The Holder runs in this process (as it does inside the Holder manager), so
//! its CPU and context switches are this process's. With `attach`, one output
//! subscriber reads and discards every frame on its own thread, which is the
//! path a live Engine takes, minus the parsing that would pace it. With
//! `bare`, no Holder at all: a thread of this process reads the same child's
//! PTY in 64 KiB blocking reads and discards them, the ceiling the PTY puts on
//! any reader.
//!
//! Usage: drainbench <payload> [none|attach|bare] [runs]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use diri_engine::holder::server::HolderServer;
use diri_engine::holder::{HolderClient, HolderLaunchSpec};

fn usage() -> (Duration, Duration, i64, i64) {
    // SAFETY: zeroed rusage is a valid out-parameter.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    (
        tv(usage.ru_utime),
        tv(usage.ru_stime),
        usage.ru_nvcsw as i64,
        usage.ru_nivcsw as i64,
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let payload =
        std::fs::canonicalize(args.next().expect("usage: drainbench <payload>")).expect("payload");
    let mode = args.next().unwrap_or_default();
    let attach = mode == "attach";
    let bare = mode == "bare";
    let runs: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1);
    let mb = std::fs::metadata(&payload).expect("payload").len() as f64 / 1e6;

    for run in 0..runs {
        let root = tempfile::tempdir().expect("tempdir");
        let start = root.path().join("start");
        let done = root.path().join("done");
        for fifo in [&start, &done] {
            let path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
            // SAFETY: a valid C path.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let script = format!(
            "read _ < '{}'; cat '{}'; echo done > '{}'; exec sleep 600",
            start.display(),
            payload.display(),
            done.display()
        );
        if bare {
            let mut pty = diri_engine::Pty::spawn(
                &diri_engine::PtySpec::new(vec!["/bin/sh".into(), "-c".into(), script], "/tmp")
                    .env("PATH", "/usr/bin:/bin")
                    .size(160, 50),
            )
            .expect("spawn pty");
            let mut reader = pty.reader().expect("pty reader");
            let drain = std::thread::spawn(move || {
                let mut buffer = vec![0u8; 64 << 10];
                let mut bytes = 0u64;
                let mut reads = 0u64;
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            bytes += count as u64;
                            reads += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                (bytes, reads)
            });
            let (drained, cpu, csw) = timed_drain(&start, &done);
            // The child idles in `sleep` holding the terminal open; the
            // reader sees its end once it is gone.
            let _ = pty.terminate(Duration::from_secs(2));
            drop(pty);
            let (bytes, reads) = drain.join().expect("drain");
            println!(
                "drain {:.3} s ({:.1} MB/s)  cpu user {:.3} sys {:.3}  csw vol {} invol {}  bare {:.1} MB in {reads} reads",
                drained.as_secs_f64(),
                mb / drained.as_secs_f64(),
                cpu.0,
                cpu.1,
                csw.0,
                csw.1,
                bytes as f64 / 1e6,
            );
            continue;
        }
        let session = format!("s_drain{run}");
        let spec = HolderLaunchSpec {
            session_id: session.clone(),
            socket_path: root.path().join("h.sock").to_string_lossy().into_owned(),
            pid_file_path: root.path().join("h.pid").to_string_lossy().into_owned(),
            log_file_path: root
                .path()
                .join(format!("{session}.bin"))
                .to_string_lossy()
                .into_owned(),
            argv: vec!["/bin/sh".into(), "-c".into(), script],
            cwd: "/tmp".into(),
            environment: [("PATH".to_string(), "/usr/bin:/bin".to_string())].into(),
            cols: 160,
            rows: 50,
            disk_capacity: 0,
        };
        let client = HolderClient::new(&spec.socket_path);
        let server = std::thread::spawn(move || HolderServer::run(spec));
        let ready = Instant::now() + Duration::from_secs(10);
        while !client.is_alive() {
            assert!(Instant::now() < ready, "holder never came up");
            std::thread::sleep(Duration::from_millis(5));
        }

        let reader = attach.then(|| {
            let mut stream = client
                .open_output_stream()
                .expect("subscribe")
                .expect("output stream supported");
            std::thread::spawn(move || {
                let mut run = Vec::new();
                let mut bytes = 0u64;
                let mut frames = 0u64;
                while let Ok(next) = stream.next_run_into(Duration::from_secs(5), 1 << 20, &mut run)
                {
                    match next {
                        Some(_) => {
                            bytes += run.len() as u64;
                            frames += 1;
                        }
                        None => break,
                    }
                }
                (bytes, frames)
            })
        });
        let (drained, cpu, csw) = timed_drain(&start, &done);

        client.kill_tree().expect("kill-tree");
        let _ = server.join();
        let received = reader.map(|reader| reader.join().expect("reader"));
        println!(
            "drain {:.3} s ({:.1} MB/s)  cpu user {:.3} sys {:.3}  csw vol {} invol {}{}",
            drained.as_secs_f64(),
            mb / drained.as_secs_f64(),
            cpu.0,
            cpu.1,
            csw.0,
            csw.1,
            received.map_or_else(String::new, |(bytes, frames)| format!(
                "  subscriber {:.1} MB in {frames} runs",
                bytes as f64 / 1e6
            )),
        );
    }
}

/// Starts the child's `cat` and waits for it to finish: the time, this
/// process's CPU (user, system) and context switches (voluntary,
/// involuntary) meanwhile.
fn timed_drain(
    start: &std::path::Path,
    done: &std::path::Path,
) -> (Duration, (f64, f64), (i64, i64)) {
    std::thread::sleep(Duration::from_millis(200));
    let before = usage();
    let began = Instant::now();
    std::fs::OpenOptions::new()
        .write(true)
        .open(start)
        .unwrap()
        .write_all(b"go\n")
        .unwrap();
    let mut line = String::new();
    std::fs::File::open(done)
        .unwrap()
        .read_to_string(&mut line)
        .unwrap();
    let drained = began.elapsed();
    let after = usage();
    (
        drained,
        (
            (after.0 - before.0).as_secs_f64(),
            (after.1 - before.1).as_secs_f64(),
        ),
        (after.2 - before.2, after.3 - before.3),
    )
}
