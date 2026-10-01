//! Manual capture tool, deliberately not a test or release assertion. See WINDOWS.md.
use diri_terminal_state::HeadlessScreen;
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scenario {
    argv: Vec<String>,
    cwd: PathBuf,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Step {
    input: Option<String>,
    size: Option<[u16; 2]>,
    settle_ms: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--emit") {
        print!(
            "\x1b[2J\x1b[H\x1b[38;2;12;120;240mTruecolor\x1b[0m\r\n\x1b]8;;https://example.com\x1b\\Hyperlink\x1b]8;;\x1b\\\r\n\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?2026hSynchronized\x1b[?2026l\x1b[>1u\x1b]52;c;ZGlyaS1maWRlbGl0eQ==\x07\r\nwide: 界 emoji: 🐈\r\n"
        );
        io::stdout().flush()?;
        std::thread::sleep(Duration::from_secs(5));
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--compare") && args.len() == 3 {
        let left: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
        let right: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[2])?)?;
        let a = left["checkpoints"]
            .as_array()
            .ok_or("missing checkpoints")?;
        let b = right["checkpoints"]
            .as_array()
            .ok_or("missing checkpoints")?;
        println!("Checkpoint counts: {} / {}", a.len(), b.len());
        for index in 0..a.len().max(b.len()) {
            for field in [
                "grid",
                "cursor",
                "altScreen",
                "bracketedPaste",
                "mouse",
                "keyboard",
                "clipboard",
                "raw",
            ] {
                let x = a.get(index).and_then(|v| v.get(field));
                let y = b.get(index).and_then(|v| v.get(field));
                println!(
                    "{index} {field}: {}",
                    if x == y {
                        "equal"
                    } else {
                        "DIFF (inspect capture JSON)"
                    }
                );
            }
        }
        return Ok(());
    }
    if args.len() != 2 {
        return Err("usage: pty_fidelity scenario.json capture.json | --compare unix.json windows.json | --emit".into());
    }
    let scenario: Scenario = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    if scenario.steps.len() > 128 {
        return Err("at most 128 checkpoints".into());
    }
    let mut spec = diri_pty::PtySpec::new(scenario.argv, scenario.cwd);
    spec.env = diri_platform::launch::local_environment();
    spec.env.retain(|(key, _)| {
        !matches!(
            key.to_ascii_uppercase().as_str(),
            "TERM" | "COLORTERM" | "NO_COLOR" | "FORCE_COLOR"
        )
    });
    spec.env.extend([
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
    ]);
    let mut pty = diri_pty::Pty::spawn(&spec)?;
    let mut output = pty.reader()?;
    output.set_nonblocking(true)?;
    let mut input = pty.writer()?;
    let mut terminal = HeadlessScreen::new_with_keyboard_enhancements(80, 24);
    let mut checkpoints = Vec::new();
    let mut eof = false;
    let mut total = 0usize;
    for step in scenario.steps {
        if step.settle_ms > 30_000 {
            return Err("checkpoint wait exceeds 30 seconds".into());
        }
        if let Some([cols, rows]) = step.size {
            pty.resize(cols, rows)?;
            terminal.resize(cols.into(), rows.into());
        }
        if let Some(bytes) = step.input {
            input.write_all(bytes.as_bytes())?;
        }
        let deadline = Instant::now() + Duration::from_millis(step.settle_ms);
        let mut raw = Vec::new();
        let mut buffer = [0; 8192];
        while !eof && Instant::now() < deadline {
            match output.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    total += n;
                    if total > 16 * 1024 * 1024 {
                        return Err("capture exceeds 16 MiB".into());
                    }
                    raw.extend_from_slice(&buffer[..n]);
                    terminal.feed(&buffer[..n]);
                    let replies = terminal.take_replies();
                    if !replies.is_empty() {
                        input.write_all(&replies)?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    output.wait_readable(Duration::from_millis(10))?;
                }
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(5) => eof = true,
                Err(error) => return Err(error.into()),
            }
        }
        checkpoints.push(serde_json::json!({
            "grid": terminal.grid_update(true).encode()?, "cursor": terminal.cursor(),
            "altScreen": terminal.is_alt_screen(), "bracketedPaste": terminal.bracketed_paste(),
            "mouse": terminal.mouse_modes(), "keyboard": format!("{:?}", terminal.keyboard_snapshot()),
            "clipboard": terminal.take_clipboard(), "raw": raw,
        }));
    }
    let exit = pty.try_wait()?;
    if exit.is_none() {
        let _ = pty.terminate(Duration::from_millis(250));
    }
    let capture = serde_json::json!({"format": 1, "platform": std::env::consts::OS, "architecture": std::env::consts::ARCH, "exit": format!("{exit:?}"), "checkpoints": checkpoints});
    let mut file = diri_platform::security::create_private(std::path::Path::new(&args[1]))?;
    file.write_all(&serde_json::to_vec_pretty(&capture)?)?;
    Ok(())
}
