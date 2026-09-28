//! What one Engine state persist costs against a realistically large
//! `state.json`.
//!
//! The fixture mirrors a long-lived install: 50 sessions, five of which carry
//! 26 pull requests with ~2 KB bodies and review discussion (the live file
//! that motivated this bench was 1.5 MB, almost all `sessions`), 20 projects,
//! and a ~60 KB `workspaceState` built through real workspace mutations.
//!
//! Measured, all in-process against a private temp directory:
//! - `persist unchanged`: `persist_for_shutdown` with no state change (what a
//!   flusher tick costs when nothing observable moved);
//! - `persist changed`: `mark_seen` then `persist_for_shutdown`;
//! - `mark_seen rpc`: one `session.mark_seen` request/response over the
//!   control socket, spaced past the persist debounce so every request pays
//!   whatever persist the request thread does;
//! - `workspace.mutate` / `workspace.snapshot`: one request/response each over
//!   the control socket.
//!
//! Wall time includes F_FULLFSYNC; CPU is process user+system time per
//! operation (getrusage), which is what a sampling profile of the Engine sees.
//!
//! Usage: statebench [iterations]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use serde_json::{Value, json};

fn engine() -> Arc<ManifestEngine> {
    let dir = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&dir).expect("load");
    Arc::new(engine)
}

fn cpu() -> Duration {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let micros = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime))
}

fn filler(seed: usize, len: usize) -> String {
    let words = [
        "refactor", "holder", "session", "persist", "terminal", "snapshot", "review", "latency",
        "fsync", "sidebar", "attach", "worktree",
    ];
    let mut out = String::with_capacity(len + 16);
    let mut i = seed;
    while out.len() < len {
        out.push_str(words[i % words.len()]);
        out.push(if i.is_multiple_of(13) { '\n' } else { ' ' });
        i = i.wrapping_mul(31).wrapping_add(7);
    }
    out
}

fn pull_request(session: usize, n: usize) -> Value {
    let discussion: Vec<Value> = (0..6)
        .map(|d| {
            json!({"kind": if d % 2 == 0 {"comment"} else {"review"}, "author": format!("user{d}"),
                "body": filler(session * 1000 + n * 10 + d, 1200), "state": "COMMENTED",
                "createdAt": 1_790_000_000_000.0 + d as f64, "url": format!("https://github.com/o/r/pull/{n}#c{d}")})
        })
        .collect();
    let checks: Vec<Value> = (0..8)
        .map(|c| json!({"name": format!("check-{c}"), "result": "success", "url": format!("https://ci/{n}/{c}")}))
        .collect();
    json!({"url": format!("https://github.com/o/r/pull/{}", 100 + n), "number": 100 + n,
        "title": format!("PR {n}: {}", filler(n, 40)), "author": "cristi",
        "body": filler(session * 100 + n, 2000), "baseRefName": "main", "headRefName": format!("feat/{n}"),
        "state": "OPEN", "isDraft": false, "reviewDecision": "REVIEW_REQUIRED", "mergeable": "MERGEABLE",
        "additions": 120, "deletions": 40, "changedFiles": 7, "commentCount": 6, "reviewCount": 3,
        "checksPassed": 8, "checksFailed": 0, "checksPending": 0, "checks": checks,
        "discussion": discussion, "fetchedAt": 1_790_000_000_000.0})
}

fn fixture(root: &std::path::Path) -> Vec<String> {
    let projects: Vec<Value> = (0..20)
        .map(|p| json!({"id": format!("project-{p}"), "root": root.join(format!("repo{p}")), "name": format!("repo{p}")}))
        .collect();
    let mut ids = Vec::new();
    let sessions: Vec<Value> = (0..50)
        .map(|s| {
            let id = format!("session-{s:03}");
            ids.push(id.clone());
            let mut record = json!({"id": id, "kind": diri_proto::AgentKind::SHELL,
                "cwd": root.join(format!("repo{}", s % 20)), "projectID": format!("project-{}", s % 20),
                "title": filler(s, 60), "titleSource": diri_proto::TitleSource::FirstPrompt,
                "originatingPrompt": filler(s + 7, 600),
                "status": diri_proto::SessionStatus::Idle, "resumability": diri_proto::Resumability::NotResumable,
                "createdAt": 1_790_000_000_000.0, "updatedAt": 1_790_000_000_000.0, "pinned": false});
            if s % 10 == 0 {
                record["pullRequests"] = Value::Array((0..26).map(|n| pull_request(s, n)).collect());
            }
            record
        })
        .collect();
    let document = json!({"version": 1, "projects": projects, "sessions": sessions});
    std::fs::write(
        root.join("state.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    ids
}

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let mut line =
            serde_json::to_vec(&json!({"id": self.next, "method": method, "params": params}))
                .unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).unwrap();
        let mut reply = String::new();
        self.reader.read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert!(reply.get("err").is_none(), "{method} failed: {reply}");
        reply["ok"].clone()
    }
}

fn report(label: &str, mut wall: Vec<Duration>, cpu_total: Duration) {
    let n = wall.len();
    wall.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{label:<22} n={n:<3} wall median {:>7.2} ms  p90 {:>7.2} ms  | cpu/op {:>7.2} ms",
        ms(wall[n / 2]),
        ms(wall[(n * 9 / 10).min(n - 1)]),
        ms(cpu_total) / n as f64
    );
}

fn main() {
    let iterations: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(40);
    let temp = tempfile::tempdir().unwrap();
    let ids = fixture(temp.path());
    let state = temp.path().join("state.json");
    let registry = Arc::new(Mutex::new(Registry::new(engine(), &state)));
    registry.lock().unwrap().load().unwrap();

    let server = Arc::new(ControlServer::new(
        Arc::clone(&registry),
        temp.path().join("daemon.sock"),
    ));
    let listener = server.bind().unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = server.serve(stream);
    });
    let writer = UnixStream::connect(temp.path().join("daemon.sock")).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut client = Client {
        reader: BufReader::new(writer.try_clone().unwrap()),
        writer,
        next: 0,
    };

    // A realistic organization section: 12 workspaces, 20 tabs each.
    let mut revision = client.call("workspace.snapshot", json!({}))["revision"]
        .as_u64()
        .unwrap();
    for w in 0..12 {
        let snapshot = client.call(
            "workspace.mutate",
            json!({"expectedRevision": revision, "mutation": {"type": "createWorkspace", "name": format!("workspace {w}")}}),
        );
        revision = snapshot["revision"].as_u64().unwrap();
        let workspace = snapshot["workspaces"].as_array().unwrap().last().unwrap()["id"].clone();
        for t in 0..20 {
            let snapshot = client.call(
                "workspace.mutate",
                json!({"expectedRevision": revision, "mutation": {"type": "createTab", "workspaceId": workspace,
                    "sessionId": ids[(w * 20 + t) % ids.len()], "title": format!("tab {t} {}", filler(t, 30).replace('\n', " ").trim())}}),
            );
            revision = snapshot["revision"].as_u64().unwrap();
        }
    }
    registry.lock().unwrap().persist_for_shutdown().unwrap();
    let document: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    println!(
        "state.json {} KB (sessions {} KB, workspaceState {} KB, projects {} KB)",
        std::fs::metadata(&state).unwrap().len() / 1024,
        serde_json::to_vec(&document["sessions"]).unwrap().len() / 1024,
        serde_json::to_vec(&document["workspaceState"])
            .unwrap()
            .len()
            / 1024,
        serde_json::to_vec(&document["projects"]).unwrap().len() / 1024,
    );

    let measure = |label: &str, iterations: usize, pause: Duration, op: &mut dyn FnMut(usize)| {
        let mut wall = Vec::with_capacity(iterations);
        let mut cpu_total = Duration::ZERO;
        for i in 0..iterations {
            std::thread::sleep(pause);
            let (cpu0, start) = (cpu(), Instant::now());
            op(i);
            wall.push(start.elapsed());
            cpu_total += cpu() - cpu0;
        }
        report(label, wall, cpu_total);
    };

    measure("persist unchanged", iterations, Duration::ZERO, &mut |_| {
        registry.lock().unwrap().persist_for_shutdown().unwrap();
    });
    measure("persist changed", iterations, Duration::ZERO, &mut |i| {
        let mut registry = registry.lock().unwrap();
        registry.mark_seen(&ids[i % ids.len()]).unwrap();
        registry.persist_for_shutdown().unwrap();
    });
    let rpc_iterations = iterations.min(20);
    measure(
        "mark_seen rpc",
        rpc_iterations,
        Duration::from_millis(600),
        &mut |i| {
            client.call(
                "session.mark_seen",
                json!({"sessionID": ids[i % ids.len()]}),
            );
        },
    );
    // Let the deferred persist land before the next phase is timed.
    std::thread::sleep(Duration::from_millis(1200));
    let workspace = client.call("workspace.snapshot", json!({}))["workspaces"][0]["id"].clone();
    measure("workspace.mutate", iterations, Duration::ZERO, &mut |i| {
        let snapshot = client.call(
            "workspace.mutate",
            json!({"expectedRevision": revision, "mutation": {"type": "renameWorkspace", "workspaceId": workspace, "name": format!("renamed {i}")}}),
        );
        revision = snapshot["revision"].as_u64().unwrap();
    });
    measure(
        "workspace.snapshot",
        iterations,
        Duration::ZERO,
        &mut |_| {
            client.call("workspace.snapshot", json!({}));
        },
    );
}
