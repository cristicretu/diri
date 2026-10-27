#![cfg(unix)]

use std::io::ErrorKind;
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};

const HELP: &str = "Usage: dirijor session spawn KIND [OPTIONS]

Arguments:
  KIND             Agent kind to launch (for example, codex)

Options:
  --cwd PATH       Working directory (defaults to the current directory)
  --worktree       Create a Git worktree for the session
  --branch NAME    Branch for the worktree
  --prompt TEXT    Initial prompt for the agent
  --title TEXT     Session title (--name is an alias)
  --host ID        Host on which to launch the agent
  --json           Print the result as JSON
  -h, --help       Show this help

Example (local):
  dirijor session spawn codex --cwd /path/to/repo --prompt \"Explain this project\"
";

fn assert_help(output: Output) {
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), HELP);
    assert_eq!(output.stderr, b"");
}

#[test]
fn spawn_help_flags_work_without_an_engine() {
    let temp = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h"] {
        assert_help(
            Command::new(env!("CARGO_BIN_EXE_dirijor"))
                .args(["session", "spawn", flag])
                .env("DIRIJOR_SOCKET", temp.path().join("missing.sock"))
                .current_dir(temp.path())
                .output()
                .unwrap(),
        );
    }
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn spawn_help_never_connects_to_an_engine() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    for flag in ["--help", "-h"] {
        assert_help(
            Command::new(env!("CARGO_BIN_EXE_dirijor"))
                .args(["session", "spawn", flag])
                .env("DIRIJOR_SOCKET", &socket)
                .current_dir(temp.path())
                .output()
                .unwrap(),
        );
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    }
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn spawn_missing_kind_keeps_its_error_without_contacting_engine() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["session", "spawn"])
        .env("DIRIJOR_SOCKET", temp.path().join("missing.sock"))
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"dirijor: session spawn requires an agent kind\n"
    );
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
