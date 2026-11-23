//! Headless macOS detached launch to first PTY output, using this binary as the helper.
#[cfg(target_os = "macos")]
fn main() {
    use diri_pty::{PtySpec, detached};
    use std::io::Read;
    use std::time::Instant;

    detached::run_helper_if_requested();
    let root = tempfile::Builder::new()
        .prefix("d-ready-")
        .tempdir_in("/tmp")
        .unwrap();
    let launcher = detached::Launcher {
        helper: std::env::var_os("DIRI_BENCH_EXEC_AGENT")
            .map_or_else(
                || std::env::current_exe().unwrap(),
                std::path::PathBuf::from,
            )
            .canonicalize()
            .unwrap(),
        rendezvous: root.path().to_owned(),
    };
    let spec = PtySpec::new(vec!["/usr/bin/printf".into(), "ready".into()], root.path());
    let mut samples = Vec::new();
    for _ in 0..25 {
        let start = Instant::now();
        let mut pty = detached::spawn(&spec, &launcher).expect("detached launch");
        let mut marker = [0; 5];
        pty.reader().unwrap().read_exact(&mut marker).unwrap();
        assert_eq!(&marker, b"ready");
        samples.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(pty.wait().unwrap(), diri_pty::Exit::Code(0));
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "25 launches to PTY marker, median {:.2} ms, p95 {:.2} ms",
        samples[12], samples[23]
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("detached_ready uses the macOS launchd transport");
}
