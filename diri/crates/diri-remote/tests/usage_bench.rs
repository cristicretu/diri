#![cfg(unix)]
//! Opt-in `host.usage` cost bench through a fake `ssh` (no real host).
//!
//! `cargo test --release -p diri-remote --test usage_bench -- --ignored --nocapture`
//!
//! Builds a remote HOME fixture with Claude and Codex transcripts, then drives
//! `RemoteManager::transcript_usage` the way the App's five-minute poll does:
//! one cold call, several unchanged warm calls, and one call after a transcript
//! append. It reports SSH commands per call, wall time, and the CPU time of
//! everything that ran "remotely" (children of this process). Every file lives
//! in a temporary directory that is removed when the bench ends.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::time::Instant;

use diri_engine::remote::executor::ProcessExecutor;
use diri_engine::remote::manager::{ArtifactCatalog, RemoteManager};
use diri_proto::HostEntry;
use diri_proto::remote_pty::{TranscriptUsageDirectory, TranscriptUsageRequest};

fn children_cpu() -> f64 {
    // SAFETY: getrusage writes one initialized struct.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn fixture(home: &Path, files: usize, lines: usize) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let date = diri_usage::transcripts::dashboard::date_label(now / 86_400);
    for index in 0..files {
        let project = home.join(format!(".claude/projects/project-{}", index % 20));
        fs::create_dir_all(&project).unwrap();
        let mut body = String::new();
        for line in 0..lines {
            let event = serde_json::json!({"type":"assistant","timestamp":format!("{date}T{:02}:00:00Z", line % 24),
                "requestId":format!("r{index}-{line}"),"message":{"id":format!("m{index}-{line}"),"model":"claude-sonnet-4-5",
                "content":"x".repeat(400),
                "usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}});
            body.push_str(&event.to_string());
            body.push('\n');
        }
        fs::write(project.join(format!("{index}.jsonl")), body).unwrap();
    }
    let sessions = home.join(".codex/sessions/2026/09/30");
    fs::create_dir_all(&sessions).unwrap();
    for index in 0..files / 4 {
        let mut body =
            serde_json::json!({"type":"turn_context","payload":{"model":"gpt-5.4"}}).to_string();
        body.push('\n');
        for line in 0..lines {
            let usage = serde_json::json!({"timestamp":format!("{date}T{:02}:00:00Z", line % 24),"type":"event_msg",
                "payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20}}}});
            body.push_str(&usage.to_string());
            body.push('\n');
        }
        fs::write(sessions.join(format!("rollout-{index}.jsonl")), body).unwrap();
    }
}

#[test]
#[ignore = "bench: run explicitly with --ignored --nocapture"]
fn host_usage_poll_cost() {
    let files = std::env::var("USAGE_BENCH_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(400);
    let lines = std::env::var("USAGE_BENCH_LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let temporary = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    let home = root.join("remote-home");
    let state = root.join("remote-state");
    let argv = root.join("argv.log");
    fs::create_dir_all(&home).unwrap();
    fixture(&home, files, lines);
    // Emulate a real login shell's startup files (nvm, oh-my-zsh, ...):
    // environment capture runs the account's login shell.
    if let Some(seconds) = std::env::var("USAGE_BENCH_RC_SLEEP")
        .ok()
        .filter(|value| !value.is_empty())
    {
        fs::create_dir_all(home.join(".config/fish")).unwrap();
        let line = format!("sleep {seconds}\n");
        for rc in [
            ".config/fish/config.fish",
            ".zshrc",
            ".zprofile",
            ".bashrc",
            ".profile",
        ] {
            fs::write(home.join(rc), &line).unwrap();
        }
    }
    let ssh = root.join("ssh");
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&ssh)
        .unwrap();
    write!(
        file,
        "#!/bin/sh\nunset CODEX_HOME CLAUDE_CONFIG_DIR ZDOTDIR\nprintf '.' >> '{}'\ncase \" $* \" in\n  *' -O exit '*) exit 0;;\nesac\nexport HOME='{}'\nexport DIRI_REMOTE_STATE_DIR='{}'\nfor last; do :; done\nexec /bin/sh -c \"$last\"",
        argv.display(),
        home.display(),
        state.display()
    )
    .unwrap();
    drop(file);
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let manager = RemoteManager::new(
        ProcessExecutor::new(ssh),
        ArtifactCatalog::from_native_helper(Path::new(env!("CARGO_BIN_EXE_diri-remote"))).unwrap(),
        root.join("control"),
    )
    .unwrap();
    let host = HostEntry {
        transport: Default::default(),
        id: "bench".into(),
        name: None,
        ssh: "bench-host".into(),
        default_cwd: None,
        node: None,
    };
    let request = TranscriptUsageRequest {
        profiles: vec![TranscriptUsageDirectory {
            provider: "claude".into(),
            config_home: "~/.claude".into(),
        }],
    };
    // Install the Helper outside the measurement, like a host that is
    // already initialized.
    manager.ensure_helper(&host).unwrap();
    let commands = || fs::read(&argv).map(|bytes| bytes.len()).unwrap_or(0);
    let measure = |label: &str| {
        let before_commands = commands();
        let before_cpu = children_cpu();
        let started = Instant::now();
        let result = manager.transcript_usage(&host, &request).unwrap();
        let wall = started.elapsed();
        let bytes = serde_json::to_vec(&result).unwrap().len();
        println!(
            "usage_bench {label}: ssh_commands={} wall_ms={:.1} remote_cpu_ms={:.1} buckets={} response_bytes={bytes} input_total={}",
            commands() - before_commands,
            wall.as_secs_f64() * 1e3,
            (children_cpu() - before_cpu) * 1e3,
            result.buckets.len(),
            result
                .buckets
                .iter()
                .map(|bucket| bucket.input + bucket.cache_read + bucket.output)
                .sum::<i64>(),
        );
        result
    };
    let cold = measure("cold");
    let mut warm = Vec::new();
    for index in 0..5 {
        warm.push(measure(&format!("warm{index}")));
    }
    for result in &warm {
        assert_eq!(result.buckets, cold.buckets);
    }
    let appended = home.join(".claude/projects/project-0/0.jsonl");
    let mut file = fs::OpenOptions::new().append(true).open(&appended).unwrap();
    let date = diri_usage::transcripts::dashboard::date_label(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            / 86_400,
    );
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"assistant","timestamp":format!("{date}T01:00:00Z"),
            "requestId":"appended","message":{"id":"appended","model":"claude-sonnet-4-5",
            "usage":{"input_tokens":7,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
    )
    .unwrap();
    drop(file);
    let after = measure("append");
    let total = |result: &diri_proto::remote_pty::TranscriptUsageResult| {
        result
            .buckets
            .iter()
            .map(|bucket| bucket.input)
            .sum::<i64>()
    };
    assert_eq!(total(&after), total(&cold) + 7);
}
