//! Opt-in latency measurements against a real in-process Engine and a real
//! PTY (`/bin/sh`, no Holder, no network):
//!
//! ```sh
//! cargo test -p diri-web --lib bench -- --ignored --nocapture
//! ```
//!
//! It spawns one shell session in a temporary Engine on a temporary socket,
//! prints the numbers, and kills the session when done.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diri_client::DaemonClient;
use diri_proto::SessionId;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::api::Api;
use crate::auth::Auth;

const HISTORY_ROWS: i64 = 3000;

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    let index = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[index]
}

struct Cleanup(Arc<Mutex<diri_engine::Registry>>, String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .terminate(&self.1, Duration::from_secs(2));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; run with --ignored --nocapture"]
async fn bench_history_page_and_diff_delivery() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let (manifests, _) =
        diri_engine::ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
            .unwrap();
    let registry = Arc::new(Mutex::new(diri_engine::Registry::new(
        Arc::new(manifests),
        temp.path().join("state.json"),
    )));
    let server = Arc::new(
        diri_engine::control::ControlServer::new(
            Arc::clone(&registry),
            temp.path().join("daemon.sock"),
        )
        .with_logs_dir(temp.path().join("logs")),
    );
    let listener = server.bind().unwrap();
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let server = Arc::clone(&serving);
            std::thread::spawn(move || {
                let _ = server.serve(stream);
            });
        }
    });
    let client = Arc::new(DaemonClient::with_socket_path(server.socket_path()));
    client.connect();
    client
        .wait_until_connected(Duration::from_secs(5))
        .await
        .unwrap();

    // Coloured, full-width scrollback, then an echoing `cat` to type into.
    let script = format!(
        "i=0; while [ $i -lt {HISTORY_ROWS} ]; do \
         printf '\\033[1;3%dmline %05d\\033[0m some ordinary text to fill a typical terminal row \\033[2mdim tail\\033[0m\\n' $((i%8)) $i; \
         i=$((i+1)); done; exec cat"
    );
    let spawned = client
        .request(
            diri_proto::Method::SESSION_SPAWN,
            Some(&json!({
                "kind": {"shell": {}},
                "cwd": temp.path(),
                "argv": ["/bin/sh", "-c", script],
                "initialCols": 120,
                "initialRows": 40,
            })),
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    let _cleanup = Cleanup(Arc::clone(&registry), id.clone());
    let session = SessionId(id.clone());

    let deadline = Instant::now() + Duration::from_secs(30);
    let history = loop {
        let page = crate::terminal::history(&client, &session, None, 1).await;
        if let Ok(page) = page
            && page["historySize"].as_i64().unwrap_or(0) >= HISTORY_ROWS - 40
        {
            break page["historySize"].as_i64().unwrap();
        }
        assert!(Instant::now() < deadline, "scrollback never filled");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let api = Arc::new(Api {
        client: Arc::clone(&client),
        auth: Auth::temporary().unwrap(),
        host_label: "bench".into(),
        inputs: Default::default(),
        tailnet: None,
    });
    let token = api.auth.token().to_owned();

    // ---- /history: 1000-line pages, end to end through HTTP. ----
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let server_task = tokio::spawn(crate::serve_connections(tcp, Arc::clone(&api)));
    let mut http = BufReader::new(TcpStream::connect(address).await.unwrap());
    let mut samples = Vec::new();
    let mut bytes = 0;
    for round in 0..120 {
        let before = history - (round % 2) * 1000;
        let request = format!(
            "GET /api/session/{id}/history?before={before}&limit=1000 HTTP/1.1\r\nHost: b\r\nX-Diri-Token: {token}\r\n\r\n"
        );
        let started = Instant::now();
        http.get_mut().write_all(request.as_bytes()).await.unwrap();
        let mut length = 0;
        loop {
            let mut line = String::new();
            http.read_line(&mut line).await.unwrap();
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.trim().parse().unwrap();
            }
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0; length];
        tokio::io::AsyncReadExt::read_exact(&mut http, &mut body)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let page: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["lines"].as_array().unwrap().len(), 1000);
        bytes = length;
        if round >= 20 {
            samples.push(elapsed);
        }
    }
    println!(
        "history 1000-line page over HTTP: p50 {:?} p95 {:?} ({} KiB JSON, {} samples, history {history})",
        percentile(&mut samples, 0.5),
        percentile(&mut samples, 0.95),
        bytes / 1024,
        samples.len()
    );

    // ---- stream: keystroke via /input to the diff that shows its echo. ----
    let stream = TcpStream::connect(address).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(
            format!(
                "GET /api/session/{id}/stream HTTP/1.1\r\nHost: b\r\nX-Diri-Token: {token}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut lines = BufReader::new(read_half).lines();
    let mut next_event = async || loop {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("event in time")
            .unwrap()
            .unwrap();
        if let Some(name) = line.strip_prefix("event: ") {
            let data = lines.next_line().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
            break (name.to_owned(), value);
        }
    };
    let (name, _) = next_event().await;
    assert_eq!(name, "snapshot");
    let mut input = BufReader::new(TcpStream::connect(address).await.unwrap());
    let mut echo = Vec::new();
    let mut scroll = Vec::new();
    let mut snapshots = 0;
    for round in 0..80 {
        use base64::Engine as _;
        // Alternate a typed character with an Enter that scrolls history.
        let key = if round % 2 == 0 { "k" } else { "\r" };
        let body =
            json!({"data": base64::engine::general_purpose::STANDARD.encode(key)}).to_string();
        let request = format!(
            "POST /api/session/{id}/input HTTP/1.1\r\nHost: b\r\nX-Diri-Token: {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let started = Instant::now();
        input.get_mut().write_all(request.as_bytes()).await.unwrap();
        loop {
            let (name, value) = next_event().await;
            if name == "snapshot" {
                snapshots += 1;
                break;
            }
            if name == "diff" && !value["lines"].as_object().unwrap().is_empty() {
                break;
            }
            assert_ne!(name, "exit");
        }
        let elapsed = started.elapsed();
        // Drain the /input response so the connection stays in step.
        loop {
            let mut line = String::new();
            input.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let mut body = [0; 11];
        tokio::io::AsyncReadExt::read_exact(&mut input, &mut body)
            .await
            .unwrap();
        if round >= 10 {
            if round % 2 == 0 {
                &mut echo
            } else {
                &mut scroll
            }
            .push(elapsed);
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    println!(
        "input -> echo diff on the stream: p50 {:?} p95 {:?}",
        percentile(&mut echo, 0.5),
        percentile(&mut echo, 0.95)
    );
    println!(
        "input -> scrolling diff on the stream: p50 {:?} p95 {:?}",
        percentile(&mut scroll, 0.5),
        percentile(&mut scroll, 0.95)
    );
    println!("snapshot events after the opening one: {snapshots}");
    server_task.abort();
    client.begin_shutdown();
}
