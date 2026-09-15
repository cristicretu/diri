//! Run inside each terminal: termcompare <payload> <results.json>.
//! Identical raw PTY writes followed by a cursor report; no child-wait polling
//! or fallback to drain-only timing. This measures parsing, not presentation.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

struct Terminal {
    file: File,
    original: libc::termios,
}

impl Terminal {
    fn open() -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .expect("tty");
        let mut original = std::mem::MaybeUninit::uninit();
        // SAFETY: the fd is open and the out pointer has the required layout.
        assert_eq!(
            unsafe { libc::tcgetattr(file.as_raw_fd(), original.as_mut_ptr()) },
            0
        );
        // SAFETY: tcgetattr initialized original on success.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // SAFETY: raw is initialized and the fd remains owned by file.
        unsafe {
            libc::cfmakeraw(&mut raw);
            assert_eq!(libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &raw), 0);
        }
        Self { file, original }
    }

    fn query(&mut self) {
        self.file.write_all(b"\x1b[6n").expect("query");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut reply = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "terminal did not answer the cursor report"
            );
            let mut fd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd points to one initialized pollfd.
            let ready = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert!(ready > 0, "cursor report timed out");
            let mut bytes = [0; 64];
            let count = self.file.read(&mut bytes).expect("reply");
            assert!(count > 0);
            reply.extend_from_slice(&bytes[..count]);
            assert!(
                reply.len() <= 256,
                "unexpected terminal input during benchmark"
            );
            if reply.ends_with(b"R") {
                assert!(reply.starts_with(b"\x1b["), "invalid cursor report");
                break;
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: original was read from this still-open tty.
        unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.original) };
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let payload_path = args.next().expect("payload");
    let result_path = args.next().expect("results.json");
    let bytes = std::fs::read(&payload_path).expect("payload");
    let mut terminal = Terminal::open();
    // SAFETY: winsize is a plain C struct with all-zero values valid.
    let mut dimensions: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: dimensions is writable and the tty fd is open.
    assert_eq!(
        unsafe { libc::ioctl(terminal.file.as_raw_fd(), libc::TIOCGWINSZ, &mut dimensions) },
        0
    );
    terminal.query();
    let mut samples = Vec::new();
    // First pass warms parsing and scrollback allocations; record five passes.
    for round in 0..6 {
        terminal
            .file
            .write_all(b"\x1b[0m\x1b[2J\x1b[H")
            .expect("clear");
        terminal.query();
        let start = Instant::now();
        for chunk in bytes.chunks(64 << 10) {
            terminal.file.write_all(chunk).expect("payload write");
        }
        terminal.query();
        if round > 0 {
            samples.push(start.elapsed().as_secs_f64());
        }
    }
    drop(terminal);
    let output = serde_json::json!({"bytes": bytes.len(), "payload": payload_path,
            "cols": dimensions.ws_col, "rows": dimensions.ws_row, "seconds": samples});
    std::fs::write(result_path, serde_json::to_vec_pretty(&output).unwrap()).expect("results");
}
