//! Ghost Desktops against a real browser. Opt-in, because they need a
//! Chromium on the machine (Playwright's headless shell or Chrome):
//!
//! ```sh
//! cargo test -p diri-engine --test sandbox_ghost -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `DIRI_SANDBOX_BROWSER=/path/to/chrome` picks the browser. Every test
//! serves its own page from a loopback listener, uses private temporary
//! state, and stops every sandbox it starts; the crash test SIGKILLs a
//! helper process that owns a sandbox and checks the browser died with it.
//!
//! `four_parallel_benchmark` prints the measurements recorded in
//! `diri/PERF.md` under "Ghost Desktops".

use std::collections::HashSet;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_engine::sandbox::ghost::{GhostBackend, find_browser};
use diri_engine::sandbox::usage::{disk_bytes, memory_bytes, process_tree};
use diri_engine::sandbox::{Caller, MicroVmBackend, PoolConfig, SandboxPool};
use diri_proto::sandbox::{SandboxAction, SandboxStartParams};
use serde_json::json;

const PAGE: &str = r#"<!doctype html><html><head><title>Ghost fixture</title>
<style>body{font:16px system-ui;margin:24px}#out{font-size:32px;color:#0a7}</style></head>
<body><h1>Counter</h1><input id="name" placeholder="name"><button id="go">Add</button>
<div id="out">0</div><ul id="list"></ul>
<script>
let n = 0;
document.getElementById('go').addEventListener('click', () => {
  n += 1;
  document.getElementById('out').textContent = String(n);
  const li = document.createElement('li');
  li.textContent = document.getElementById('name').value + ' #' + n;
  document.getElementById('list').appendChild(li);
});
document.getElementById('name').addEventListener('keydown', e => {
  if (e.key === 'Enter') document.getElementById('go').click();
});
</script></body></html>"#;

/// A tiny HTTP server on loopback serving `PAGE` for any path.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
            });
        }
    });
    format!("http://{address}/")
}

fn pool(root: &Path, capacity: usize) -> Option<SandboxPool> {
    let binary = find_browser()?;
    eprintln!("browser: {}", binary.path.display());
    Some(SandboxPool::new(
        PoolConfig {
            capacity,
            state_root: root.join("state"),
            artifact_root: root.join("artifacts"),
            sleep_after: Duration::from_secs(3600),
            reclaim_after: Duration::from_secs(7200),
            reap_interval: Duration::from_millis(250),
        },
        vec![
            Box::new(GhostBackend::with_binary(Some(binary))),
            Box::new(MicroVmBackend),
        ],
    ))
}

fn alive(pid: u32) -> bool {
    memory_bytes(pid).is_some()
}

#[test]
#[ignore = "needs a Chromium; run with --ignored"]
fn drives_a_page_and_proves_the_change() {
    let root = tempfile::tempdir().unwrap();
    let Some(pool) = pool(root.path(), 4) else {
        eprintln!("skipped: no browser");
        return;
    };
    let url = serve();
    let info = pool
        .start(
            Caller::Session("s1"),
            &SandboxStartParams {
                url: Some(url.clone()),
                width: Some(800),
                height: Some(600),
                ..SandboxStartParams::default()
            },
        )
        .unwrap();
    assert_eq!(info.title, "Ghost fixture");
    let id = info.id.clone();
    let me = Caller::Session("s1");
    let before = pool.snapshot(me, &id, Some("before"), true).unwrap();
    assert_eq!((before.width, before.height), (800, 600));
    assert!(before.jpeg.is_some());
    pool.act(
        me,
        &id,
        &SandboxAction::Click {
            x: None,
            y: None,
            selector: Some("#name".into()),
        },
    )
    .unwrap();
    pool.act(me, &id, &SandboxAction::Type { text: "ada".into() })
        .unwrap();
    pool.act(
        me,
        &id,
        &SandboxAction::Press {
            key: "Enter".into(),
        },
    )
    .unwrap();
    pool.act(
        me,
        &id,
        &SandboxAction::Click {
            x: None,
            y: None,
            selector: Some("#go".into()),
        },
    )
    .unwrap();
    let out = pool
        .act(
            me,
            &id,
            &SandboxAction::Eval {
                expression: "[document.getElementById('out').textContent, \
                             document.querySelector('#list li').textContent]"
                    .into(),
            },
        )
        .unwrap();
    assert_eq!(out["value"], json!(["2", "ada #1"]));
    let after = pool.snapshot(me, &id, Some("after"), false).unwrap();
    assert_ne!(before.sha256, after.sha256, "the change is visible");
    // Another session can't touch it.
    assert!(
        pool.act(Caller::Session("s2"), &id, &SandboxAction::Wait { ms: 0 })
            .is_err()
    );
    // The live preview produces a frame once someone asks.
    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        let frame = pool.frame(&id, None).unwrap();
        if frame.jpeg.is_some() || Instant::now() > deadline {
            break frame;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(frame.jpeg.is_some(), "a preview frame arrived");
    assert!(frame.width > 0);
    let listed = pool.list();
    let usage = listed.sandboxes[0].usage;
    assert!(usage.memory_bytes > 0 && usage.processes >= 1);
    let pid_tree: Vec<u32> = {
        // The browser is the tree root; find it through the pool's usage.
        let pids = process_tree(std::process::id());
        pids.into_iter()
            .filter(|pid| *pid != std::process::id())
            .collect()
    };
    pool.stop(me, &id).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    for pid in pid_tree {
        assert!(!alive(pid), "pid {pid} survived stop");
    }
    assert_eq!(
        std::fs::read_dir(root.path().join("state"))
            .unwrap()
            .count(),
        0,
        "profile removed"
    );
}

#[test]
#[ignore = "needs a Chromium; run with --ignored"]
fn an_engine_crash_leaks_no_browser() {
    if std::env::var_os("DIRI_SANDBOX_CRASH_HELPER").is_some() {
        // The helper: start a sandbox, report the browser pid, wait to die.
        let root = std::env::var("DIRI_SANDBOX_CRASH_ROOT").unwrap();
        let pool = pool(Path::new(&root), 1).unwrap();
        pool.start(Caller::Person, &SandboxStartParams::default())
            .unwrap();
        let children: Vec<u32> = process_tree(std::process::id())
            .into_iter()
            .skip(1)
            .collect();
        println!("PIDS {children:?}");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    if find_browser().is_none() {
        eprintln!("skipped: no browser");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "an_engine_crash_leaks_no_browser",
            "--ignored",
            "--nocapture",
        ])
        .env("DIRI_SANDBOX_CRASH_HELPER", "1")
        .env("DIRI_SANDBOX_CRASH_ROOT", root.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(helper.stdout.take().unwrap());
    let mut line = String::new();
    let pids: Vec<u32> = loop {
        line.clear();
        assert!(stdout.read_line(&mut line).unwrap() > 0, "helper exited");
        if let Some(list) = line.trim().strip_prefix("PIDS ") {
            break serde_json::from_str(list).unwrap();
        }
    };
    assert!(!pids.is_empty());
    assert!(pids.iter().all(|pid| alive(*pid)));
    // SIGKILL: no destructor, no shutdown, no reaper.
    helper.kill().unwrap();
    helper.wait().unwrap();
    let killed = Instant::now();
    let deadline = killed + Duration::from_secs(5);
    while pids.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survivors: Vec<_> = pids.iter().filter(|pid| alive(**pid)).collect();
    eprintln!(
        "browser tree ({} processes) gone {} ms after the Engine was killed",
        pids.len(),
        killed.elapsed().as_millis()
    );
    assert!(survivors.is_empty(), "survivors: {survivors:?}");
    // What's left on disk is swept by the next pool.
    let state = root.path().join("state");
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 1);
    assert_eq!(diri_engine::sandbox::sweep_stale_state(&state), 1);
    let mut rest = String::new();
    let _ = stdout.read_to_string(&mut rest);
}

#[derive(Default, Clone)]
struct Row {
    id: String,
    spin_up_ms: u64,
    loaded_mb: f64,
    settled_mb: f64,
    trimmed_mb: f64,
    processes: u32,
    disk_kb: f64,
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

/// The frontmost app's LaunchServices id: the one that has the person's
/// keyboard focus.
#[cfg(target_os = "macos")]
fn front_app() -> String {
    std::process::Command::new("lsappinfo")
        .arg("front")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn front_app() -> String {
    String::new()
}

#[test]
#[ignore = "benchmark; needs a Chromium; run with --ignored --nocapture"]
fn four_parallel_benchmark() {
    let root = tempfile::tempdir().unwrap();
    let Some(pool) = pool(root.path(), 4) else {
        eprintln!("skipped: no browser");
        return;
    };
    let url = serve();
    let state_root = root.path().join("state");
    let disk_before = disk_bytes(&state_root);
    let front_before = front_app();
    // Warm the binary into the page cache once, as a running Mac has it.
    let warm = pool
        .start(Caller::Person, &SandboxStartParams::default())
        .unwrap();
    pool.stop(Caller::Person, &warm.id).unwrap();

    let started = Instant::now();
    let rows = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..4)
        .map(|n| {
            let pool = pool.clone();
            let url = url.clone();
            let rows = Arc::clone(&rows);
            std::thread::spawn(move || {
                let info = pool
                    .start(
                        Caller::Person,
                        &SandboxStartParams {
                            url: Some(format!("{url}?n={n}")),
                            label: Some(format!("bench-{n}")),
                            ..SandboxStartParams::default()
                        },
                    )
                    .unwrap();
                rows.lock().unwrap().push(Row {
                    id: info.id,
                    spin_up_ms: info.spin_up_ms,
                    ..Row::default()
                });
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let all_ready_ms = started.elapsed().as_millis();
    assert_eq!(pool.occupied(), 4);
    assert!(
        pool.start(Caller::Person, &SandboxStartParams::default())
            .is_err(),
        "a fifth start is refused"
    );

    let mut rows = rows.lock().unwrap().clone();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let listed = pool.list();
    for row in &mut rows {
        let info = listed.sandboxes.iter().find(|s| s.id == row.id).unwrap();
        row.loaded_mb = mb(info.usage.memory_bytes);
        row.processes = info.usage.processes;
    }

    // Parallel interaction: every sandbox clicks and reads back at once.
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let interact_started = Instant::now();
    let threads: Vec<_> = rows
        .iter()
        .map(|row| {
            let pool = pool.clone();
            let id = row.id.clone();
            let latencies = Arc::clone(&latencies);
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let t = Instant::now();
                    pool.act(
                        Caller::Person,
                        &id,
                        &SandboxAction::Click {
                            x: None,
                            y: None,
                            selector: Some("#go".into()),
                        },
                    )
                    .unwrap();
                    latencies
                        .lock()
                        .unwrap()
                        .push(t.elapsed().as_secs_f64() * 1000.0);
                }
                let out = pool
                    .act(
                        Caller::Person,
                        &id,
                        &SandboxAction::Eval {
                            expression: "document.getElementById('out').textContent".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(out["value"], "50");
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let interact_ms = interact_started.elapsed().as_millis();
    let mut latencies = latencies.lock().unwrap().clone();
    latencies.sort_by(f64::total_cmp);

    // Snapshot cost.
    let mut snapshot_ms = Vec::new();
    for row in &rows {
        let t = Instant::now();
        pool.snapshot(Caller::Person, &row.id, Some("bench"), false)
            .unwrap();
        snapshot_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    snapshot_ms.sort_by(f64::total_cmp);

    std::thread::sleep(Duration::from_secs(3));
    let listed = pool.list();
    for row in &mut rows {
        let info = listed.sandboxes.iter().find(|s| s.id == row.id).unwrap();
        row.settled_mb = mb(info.usage.memory_bytes);
        row.disk_kb = info.usage.disk_bytes as f64 / 1024.0;
    }
    // Idle trim, as the reaper does after a quiet spell.
    for row in &rows {
        pool.sleep(&row.id).unwrap();
    }
    std::thread::sleep(Duration::from_secs(2));
    let listed = pool.list();
    for row in &mut rows {
        let info = listed.sandboxes.iter().find(|s| s.id == row.id).unwrap();
        row.trimmed_mb = mb(info.usage.memory_bytes);
    }
    let disk_running = disk_bytes(&state_root).saturating_sub(disk_before);

    let front_after = front_app();
    let browsers = process_tree(std::process::id()).len() - 1;
    let tree_before_stop: Vec<u32> = process_tree(std::process::id())
        .into_iter()
        .skip(1)
        .collect();
    let stop_started = Instant::now();
    for row in &rows {
        pool.stop(Caller::Person, &row.id).unwrap();
    }
    let stop_ms = stop_started.elapsed().as_millis();
    std::thread::sleep(Duration::from_millis(100));
    let survivors: Vec<_> = tree_before_stop.iter().filter(|pid| alive(**pid)).collect();
    let disk_after = disk_bytes(&state_root).saturating_sub(disk_before);

    println!("\n## Ghost Desktops: 4 in parallel\n");
    println!(
        "| sandbox | spin-up ms | RSS loaded MB | RSS settled MB | RSS trimmed MB | processes | private disk KB |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    for row in &rows {
        println!(
            "| {} | {} | {:.1} | {:.1} | {:.1} | {} | {:.0} |",
            row.id,
            row.spin_up_ms,
            row.loaded_mb,
            row.settled_mb,
            row.trimmed_mb,
            row.processes,
            row.disk_kb
        );
    }
    let total = |f: fn(&Row) -> f64| rows.iter().map(f).sum::<f64>();
    println!(
        "| **total** | | {:.1} | {:.1} | {:.1} | {} | {:.0} |",
        total(|r| r.loaded_mb),
        total(|r| r.settled_mb),
        total(|r| r.trimmed_mb),
        rows.iter().map(|r| r.processes).sum::<u32>(),
        total(|r| r.disk_kb)
    );
    println!("\nall four ready (parallel start incl. first navigation): {all_ready_ms} ms");
    println!(
        "parallel clicks: {} actions in {interact_ms} ms, p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms",
        latencies.len(),
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.95),
        latencies.last().copied().unwrap_or_default()
    );
    println!(
        "PNG snapshot 1280x800: p50 {:.1} ms, max {:.1} ms",
        percentile(&snapshot_ms, 0.5),
        snapshot_ms.last().copied().unwrap_or_default()
    );
    println!(
        "state-root delta: {:.0} KB running, {} B after stop",
        disk_running as f64 / 1024.0,
        disk_after
    );
    println!(
        "stop all four: {stop_ms} ms; surviving processes: {}",
        survivors.len()
    );
    println!(
        "frontmost app before/after 4 sandboxes x 51 actions: {front_before} / {front_after} ({browsers} sandbox processes)"
    );
    assert_eq!(front_before, front_after, "a sandbox took focus");
    assert!(survivors.is_empty(), "survivors: {survivors:?}");
    assert_eq!(disk_after, 0);
}

#[test]
#[ignore = "needs a Chromium; run with --ignored"]
fn the_reaper_reclaims_an_ended_owner() {
    let root = tempfile::tempdir().unwrap();
    let Some(pool) = pool(root.path(), 2) else {
        eprintln!("skipped: no browser");
        return;
    };
    let live = Arc::new(Mutex::new(HashSet::from(["s1".to_owned()])));
    let check = Arc::clone(&live);
    pool.set_liveness(Arc::new(move |id: &str| check.lock().unwrap().contains(id)));
    pool.start(Caller::Session("s1"), &SandboxStartParams::default())
        .unwrap();
    let tree: Vec<u32> = process_tree(std::process::id())
        .into_iter()
        .skip(1)
        .collect();
    live.lock().unwrap().clear();
    let deadline = Instant::now() + Duration::from_secs(5);
    while pool.occupied() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(pool.occupied(), 0);
    std::thread::sleep(Duration::from_millis(100));
    assert!(tree.iter().all(|pid| !alive(*pid)));
}

/// Writes one live preview frame from each of four parallel sandboxes to
/// `$DIRI_SANDBOX_FRAMES_OUT/{1..4}.jpg`, for the app's settings screenshot
/// fixture (`DIRI_VISUAL_SANDBOX_FRAMES`).
#[test]
#[ignore = "writes preview fixtures; needs a Chromium"]
fn capture_preview_frames() {
    use base64::Engine as _;
    let Some(out) = std::env::var_os("DIRI_SANDBOX_FRAMES_OUT") else {
        eprintln!("set DIRI_SANDBOX_FRAMES_OUT");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let Some(pool) = pool(root.path(), 4) else {
        return;
    };
    let style = "body{font:15px -apple-system,system-ui;margin:0;color:#1d1d1f;background:#fafafa}\
                 header{padding:14px 28px;background:#fff;border-bottom:1px solid #e5e5e5;font-weight:600}\
                 main{padding:28px;max-width:720px}button{font:inherit;padding:9px 18px;border-radius:8px;border:0;background:#0a7cff;color:#fff}\
                 input{font:inherit;padding:9px 12px;border:1px solid #ccc;border-radius:8px;width:280px;margin:6px 0}\
                 .card{background:#fff;border:1px solid #e5e5e5;border-radius:12px;padding:18px;margin:12px 0}";
    let pages = [
        format!("<meta charset=utf-8><style>{style}</style><header>Acme Store · Checkout</header><main><h2>Order summary</h2><div class=card>Wireless headphones <b style=float:right>$129.00</b></div><div class=card>Shipping <b style=float:right>$4.99</b></div><button>Pay $133.99</button></main>"),
        format!("<meta charset=utf-8><style>{style}</style><header>Create your account</header><main><h2>Sign up</h2><input placeholder=Email value=ada@example.com><br><input type=password value=secret12><br><label><input type=checkbox checked style=width:auto> Accept terms</label><br><br><button>Create account</button></main>"),
        format!("<meta charset=utf-8><style>{style}</style><header>Metrics</header><main><h2>Weekly active users</h2><div class=card style=height:150px;display:flex;align-items:flex-end;gap:10px>{}</div><div class=card>Retention 41% · Churn 3.2%</div></main>",
            [40, 65, 52, 80, 95, 70, 110].iter().map(|h| format!("<div style='width:36px;height:{h}px;background:#0a7cff;border-radius:6px'></div>")).collect::<String>()),
        format!("<meta charset=utf-8><style>{style}</style><header>Docs</header><main><input value='sandbox' style=width:420px><div class=card><b>sandbox.start</b><br>Start a Ghost Desktop leased to the caller.</div><div class=card><b>sandbox.snapshot</b><br>Save a PNG for before/after proof.</div></main>"),
    ];
    let ids: Vec<String> = pages
        .iter()
        .map(|html| {
            let url = format!(
                "data:text/html;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(html)
            );
            pool.start(
                Caller::Person,
                &SandboxStartParams {
                    url: Some(url),
                    ..SandboxStartParams::default()
                },
            )
            .unwrap()
            .id
        })
        .collect();
    std::fs::create_dir_all(&out).unwrap();
    for (index, id) in ids.iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let jpeg = loop {
            if let Some(jpeg) = pool.frame(id, None).unwrap().jpeg {
                break jpeg;
            }
            assert!(Instant::now() < deadline, "no frame from {id}");
            std::thread::sleep(Duration::from_millis(50));
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(jpeg)
            .unwrap();
        std::fs::write(Path::new(&out).join(format!("{}.jpg", index + 1)), bytes).unwrap();
    }
    pool.shutdown();
}
