//! Cost of `session.updated` delivery, from producer to a subscribed client.
//!
//! Replays the publication mix recorded from a live Engine over 20 seconds
//! (51 sessions, four agents working): 101 `session.updated` publications, of
//! which 14 changed anything. Record sizes follow that capture: one session
//! tracks 26 pull requests (~270 KB encoded), one 10 (~105 KB), one 3, and
//! two have none (~1.2 KB). All text is synthetic.
//!
//! The publications go through the production `ControlServer` over a Unix
//! socket pair, subscribed with `events.subscribe`, and the client decodes
//! every frame the way the desktop app does (`ControlMessage`, then
//! `SessionRecord`). Engine CPU is process CPU minus the client thread's.
//!
//! Usage: eventbench [rounds] [distinct]   (one round = the 20-second mix)
//!
//! `distinct` makes every publication a change, isolating the per-event
//! delivery cost from the suppression of identical restatements.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_proto::{ControlMessage, SessionRecord};
use serde_json::{Value, json};

fn thread_cpu() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn process_cpu() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let seconds = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn text(len: usize, seed: usize) -> String {
    (0..len)
        .map(|i| (b'a' + ((i * 7 + seed) % 26) as u8) as char)
        .collect()
}

fn pull_request(n: usize) -> Value {
    json!({
        "url": format!("https://example.invalid/pull/{n}"),
        "number": n,
        "title": text(56, n),
        "state": "MERGED",
        "isDraft": false,
        "author": "someone",
        "headRefName": text(28, n + 1),
        "baseRefName": "main",
        "body": text(2360, n + 2),
        "additions": 15, "deletions": 5, "changedFiles": 5,
        "mergeable": "UNKNOWN", "mergeStateStatus": "UNKNOWN",
        "checksPassed": 6, "checksFailed": 0, "checksPending": 0,
        "checks": (0..6).map(|c| json!({
            "name": format!("CI / job {c}"), "result": "pass", "detail": "SUCCESS",
            "url": format!("https://example.invalid/actions/runs/{n}/job/{c}")
        })).collect::<Vec<_>>(),
        "reviewCount": 0, "commentCount": 1, "resolvedThreads": 0, "totalThreads": 0,
        "discussion": [{ "kind": "comment", "author": "bot", "body": text(1000, n + 3) }],
        "fetchedAt": 1790587289861.408,
    })
}

fn record(index: usize, pull_requests: usize, version: u64) -> SessionRecord {
    let mut value = json!({
        "agentSessionID": text(36, index),
        "capabilities": { "archive": true, "fork": true, "quickApprove": true,
                          "reliableCompletion": true, "resume": false, "sendText": true },
        "createdAt": 1790587097323.374,
        "cwd": "/Users/someone/project",
        "id": format!("s_bench{index}"),
        "kind": { "claudeCode": {} },
        "lastSeenAt": 1790587319570.3772,
        "memoryBytes": 482_552_232u64 + version,
        "pinned": false,
        "projectID": "p_bench",
        "resumability": "live",
        "status": { "working": {} },
        "title": text(40, index),
        "titleSource": 2,
        "transcriptPath": "/Users/someone/.claude/projects/p/t.jsonl",
        "updatedAt": 1790587129185.6638,
    });
    if pull_requests > 0 {
        value["pullRequests"] = (0..pull_requests).map(pull_request).collect();
    }
    serde_json::from_value(value).expect("synthetic record")
}

fn main() {
    let rounds: u64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(20);
    let distinct = std::env::args().any(|arg| arg == "distinct");
    // (pull requests, publications per round, of which changed)
    let sessions = [(26, 27, 2), (10, 21, 2), (0, 21, 4), (0, 28, 5), (3, 4, 1)];

    let temp = tempfile::tempdir().expect("temp");
    let (engine, _) =
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir()).expect("manifests");
    let registry = Registry::new(Arc::new(engine), temp.path().join("state.json"));
    let server = Arc::new(ControlServer::new(
        Arc::new(Mutex::new(registry)),
        temp.path().join("daemon.sock"),
    ));
    let events = server.events();
    let (client, engine_end) = UnixStream::pair().expect("pair");
    {
        let server = Arc::clone(&server);
        std::thread::spawn(move || server.serve(engine_end));
    }

    let mut writer = client.try_clone().expect("clone");
    writeln!(
        writer,
        "{}",
        json!({ "id": 1, "method": "events.subscribe", "params": {} })
    )
    .unwrap();
    let mut reader = BufReader::new(client);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).unwrap();

    let client_thread = std::thread::spawn(move || {
        let (mut frames, mut bytes, mut records) = (0u64, 0u64, 0u64);
        let started = thread_cpu();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).unwrap() == 0 {
                break;
            }
            frames += 1;
            bytes += line.len() as u64;
            let Ok(ControlMessage::Event { name, params, .. }) =
                serde_json::from_slice::<ControlMessage>(&line)
            else {
                continue;
            };
            if name == "bench.done" {
                break;
            }
            if serde_json::from_value::<SessionRecord>(params).is_ok() {
                records += 1;
            }
        }
        (frames, bytes, records, thread_cpu() - started)
    });

    let cpu_before = process_cpu();
    let wall = Instant::now();
    let mut version = 0u64;
    let mut current: Vec<SessionRecord> = sessions
        .iter()
        .enumerate()
        .map(|(index, (prs, _, _))| record(index, *prs, 0))
        .collect();
    let longest = sessions.iter().map(|s| s.1).max().unwrap();
    let mut published = 0u64;
    for _ in 0..rounds {
        // Interleave sessions as the live stream did; spread the changes.
        for step in 0..longest {
            for (index, (prs, count, changed)) in sessions.iter().enumerate() {
                if step >= *count {
                    continue;
                }
                if distinct
                    || (step % (count / changed) == 0 && step / (count / changed) < *changed)
                {
                    version += 1;
                    current[index] = record(index, *prs, version);
                }
                let id = current[index].id.0.clone();
                events.publish_encoded("session.updated", &current[index], Some(&id));
                published += 1;
            }
        }
    }
    events.publish("bench.done", json!({}), None);
    let (frames, bytes, records, client_cpu) = client_thread.join().unwrap();
    let elapsed = wall.elapsed().as_secs_f64();
    let engine_cpu = process_cpu() - cpu_before - client_cpu;

    println!("rounds {rounds} (each = 20 s of the recorded live mix), published {published}");
    println!("frames delivered   {frames:>10}");
    println!("records decoded    {records:>10}");
    println!("bytes on the wire  {:>10.1} MB", bytes as f64 / 1e6);
    println!("wall               {:>10.1} ms", elapsed * 1e3);
    println!(
        "engine CPU         {:>10.1} ms  ({:.2} ms per 20 s of live traffic)",
        engine_cpu * 1e3,
        engine_cpu * 1e3 / rounds as f64
    );
    println!(
        "client decode CPU  {:>10.1} ms  ({:.2} ms per 20 s of live traffic)",
        client_cpu * 1e3,
        client_cpu * 1e3 / rounds as f64
    );
}
