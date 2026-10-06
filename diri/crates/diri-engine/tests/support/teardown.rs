//! Teardown shared by the opt-in real-Agent tests, whose Agents can leave
//! detached processes in the temporary HOME. One that outlives that HOME
//! keeps running until reboot, so every step reports what it could not
//! clean up instead of ignoring it.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STOP_WITHIN: Duration = Duration::from_secs(10);
const EXIT_WITHIN: Duration = Duration::from_secs(5);

/// Runs an Agent's own daemon stop command against `home`, waiting at most
/// `STOP_WITHIN`. `env_remove` names the variable that would aim the stop at
/// the developer's own daemon instead.
pub fn stop_daemon(program: &Path, args: &[&str], home: &Path, env_remove: &str) {
    let command = format!("{} {}", program.display(), args.join(" "));
    let mut child = match Command::new(program)
        .args(args)
        .env("HOME", home)
        .env_remove(env_remove)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("teardown: `{command}` did not start: {error}");
            return;
        }
    };
    let deadline = Instant::now() + STOP_WITHIN;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if status.is_some_and(|status| status.success()) {
        return;
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
    }
    let outcome = status.map_or_else(
        || format!("timed out after {STOP_WITHIN:?}"),
        |status| status.to_string(),
    );
    eprintln!("teardown: `{command}` {outcome}: {}", stderr.trim());
}

/// Waits for every process naming `root` in its command line to exit, then
/// kills and reports any that remain.
pub fn sweep(root: &Path) {
    let deadline = Instant::now() + EXIT_WITHIN;
    let mut left = processes_under(root);
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        left = processes_under(root);
    }
    for (pid, command) in &left {
        eprintln!("teardown: killing leftover {pid} {command}");
        // SAFETY: plain kill(2) on a pid just listed under this test's root.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
}

/// Removes `root`, retrying while processes still write into it. Leaves the
/// `TempDir` that owns it nothing to do.
pub fn remove_tree(root: &Path) {
    let deadline = Instant::now() + EXIT_WITHIN;
    loop {
        match std::fs::remove_dir_all(root) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) if Instant::now() >= deadline => {
                eprintln!("teardown: {} left behind: {error}", root.display());
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Processes whose command line contains `root`'s unique final component,
/// which matches it whether spelled through `/var` or `/private/var`.
fn processes_under(root: &Path) -> Vec<(i32, String)> {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Ok(output) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };
    let own = std::process::id() as i32;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, command) = line.trim_start().split_once(' ')?;
            let pid = pid.parse().ok()?;
            (pid != own && command.contains(name)).then(|| (pid, command.to_string()))
        })
        .collect()
}
