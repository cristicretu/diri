//! Native ConPTY sessions: an interactive shell stays alive, receives input
//! and produces output, and its Job tree ends with the session.
#![cfg(windows)]

use diri_pty::{Pty, PtySpec};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn environment() -> Vec<(String, String)> {
    [
        "SystemRoot",
        "SystemDrive",
        "PATH",
        "TEMP",
        "TMP",
        "USERPROFILE",
    ]
    .into_iter()
    .filter_map(|key| Some((key.to_owned(), std::env::var(key).ok()?)))
    .collect()
}

fn spawn(argv: &[&str]) -> Pty {
    let mut spec = PtySpec::new(
        argv.iter().map(|arg| (*arg).to_owned()).collect(),
        std::env::temp_dir(),
    );
    spec.env = environment();
    Pty::spawn(&spec).expect("spawn")
}

/// Reads until `needle` appears in the output, the child exits, or the
/// deadline passes. Returns everything read.
fn read_until(pty: &mut Pty, needle: &str, timeout: Duration) -> String {
    let mut reader = pty.reader().expect("reader");
    reader.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let _ = reader.wait_readable(Duration::from_millis(50));
            }
            Err(e) => panic!("read: {e}"),
        }
        if String::from_utf8_lossy(&output).contains(needle) {
            break;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[test]
fn an_interactive_cmd_stays_alive_and_echoes_input() {
    let comspec = std::env::var("ComSpec").expect("ComSpec");
    let mut pty = spawn(&[&comspec, "/d", "/q", "/k"]);
    read_until(&mut pty, ">", Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(500));
    assert!(pty.try_wait().unwrap().is_none(), "cmd exited while idle");
    pty.writer().unwrap().write_all(b"set /a 40+2\r").unwrap();
    // The echoed command reads "40+2"; only evaluation prints 42.
    let output = read_until(&mut pty, "42", Duration::from_secs(5));
    assert!(output.contains("42"), "{output:?}");
    pty.kill_group(9).unwrap();
    pty.wait().unwrap();
}

#[test]
fn an_interactive_powershell_stays_alive_and_echoes_input() {
    let mut pty = spawn(&["powershell.exe", "-NoLogo", "-NoProfile"]);
    read_until(&mut pty, "PS ", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        pty.try_wait().unwrap().is_none(),
        "powershell exited while idle"
    );
    pty.writer()
        .unwrap()
        .write_all(b"'diri' + '-ok'\r")
        .unwrap();
    let output = read_until(&mut pty, "diri-ok", Duration::from_secs(10));
    assert!(output.contains("diri-ok"), "{output:?}");
    pty.kill_group(9).unwrap();
    pty.wait().unwrap();
}
