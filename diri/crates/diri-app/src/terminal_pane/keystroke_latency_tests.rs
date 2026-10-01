//! Keystroke → echo → drawn frame, hop by hop, through the real pane.
//!
//! A private in-process Engine (with a real Holder when `DIRI_HOLDER_BIN`
//! names one, otherwise the direct PTY path) runs `cat`, so the TTY line
//! discipline echoes and nothing but Diri sits on the path. A `TerminalPane`
//! in a headless GPUI window attaches to it over the production client and
//! transport, and each key goes through the pane's own key handler. The
//! window renders through the real headless Metal renderer; a headless
//! renderer never presents, so records close at GPU completion.
//!
//! The GPUI thread here is a deterministic test dispatcher that this harness
//! pumps in a busy loop, so "mailbox queued → grid applied" measures that
//! loop, not the main run loop's queueing, and there is no display link:
//! a frame is drawn in the effect flush that dirtied it. Waiting for the
//! display's next refresh and the compositor are therefore not in these
//! numbers; `echo_frame_scheduling_against_the_display_link` measures the
//! first on this Mac's real CVDisplayLink.
//!
//! ```sh
//! cargo test -p diri-app --release --bin diri keystroke_latency -- --ignored --nocapture
//! ```
//!
//! `DIRI_KEY_LATENCY_SAMPLES` sets the key count (default 200). Cleanup: the
//! session is killed, this run's Holder manager (matched by its unique temp
//! root) is stopped, and the root removed.

use diri_platform::ipc::UnixStream;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

use diri_client::latency_trace;
use diri_engine::control::ControlServer;
use diri_engine::detect::ManifestEngine;
use diri_engine::registry::Registry;
use diri_engine::session::HolderConfig;
use diri_proto::ControlMessage;
use gpui::{HeadlessAppContext, KeyDownEvent, Keystroke, px, size};
use serde_json::json;

use super::*;

struct PrivateEngine {
    root: PathBuf,
    socket: PathBuf,
    control: UnixStream,
    replies: std::io::BufReader<UnixStream>,
    next_id: u64,
}

impl PrivateEngine {
    fn start() -> Self {
        let dir = diri_engine::detect::bundled_manifest_dir()
            .canonicalize()
            .expect("manifests");
        let (engine, _) = ManifestEngine::load_dir(&dir).expect("load manifests");
        // Short root: Holder sockets live under it and must fit SUN_LEN.
        let root = PathBuf::from(format!("/tmp/diri-applat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let registry = Arc::new(StdMutex::new(Registry::new(
            Arc::new(engine),
            root.join("state.json"),
        )));
        let mut server = ControlServer::new(Arc::clone(&registry), root.join("daemon.sock"))
            .with_logs_dir(root.join("logs"));
        if let Some(holder) = std::env::var_os("DIRI_HOLDER_BIN") {
            server = server.with_holder(HolderConfig {
                holders_dir: root.join("holders"),
                executable: PathBuf::from(holder),
            });
        }
        let server = Arc::new(server);
        let listener = server.bind().expect("bind");
        {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    let server = Arc::clone(&server);
                    std::thread::spawn(move || {
                        let _ = server.serve(stream);
                    });
                }
            });
        }
        let socket = server.socket_path().to_path_buf();
        let control = UnixStream::connect(&socket).expect("connect control");
        let replies = std::io::BufReader::new(control.try_clone().expect("clone"));
        Self {
            root,
            socket,
            control,
            replies,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut bytes = serde_json::to_vec(&ControlMessage::Request {
            id,
            method: method.into(),
            params: Some(params),
        })
        .expect("encode");
        bytes.push(b'\n');
        self.control.write_all(&bytes).expect("write");
        loop {
            let mut line = String::new();
            self.replies.read_line(&mut line).expect("reply");
            if let Ok(ControlMessage::Response {
                id: reply, result, ..
            }) = serde_json::from_str(&line)
                && reply == id
            {
                return result.unwrap_or_else(|error| panic!("{method} failed: {error:?}"));
            }
        }
    }
}

impl Drop for PrivateEngine {
    fn drop(&mut self) {
        // The Holder manager outlives an in-process Engine by design; this
        // root is unique to the run, so only its own Holders match.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(self.root.join("holders"))
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn typed(key: &str) -> KeyDownEvent {
    let mut keystroke = Keystroke::parse(key).unwrap();
    if key.len() == 1 {
        keystroke.key_char = Some(key.into());
    }
    KeyDownEvent {
        keystroke,
        is_held: false,
        prefer_character_input: false,
    }
}

#[test]
#[ignore = "measurement: keystroke latency by hop; run explicitly on macOS"]
fn keystroke_latency_by_hop_through_the_pane() {
    let samples: usize = std::env::var("DIRI_KEY_LATENCY_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    latency_trace::set_enabled(true);
    latency_trace::set_expects_present(false);
    crate::telemetry::install_latency_trace();

    let mut engine = PrivateEngine::start();
    let spawned = engine.call(
        "session.spawn",
        json!({
            "kind": { "shell": {} },
            "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", "printf 'ready\\n'; exec cat"],
        }),
    );
    let id = SessionId(spawned["id"].as_str().expect("id").to_owned());
    let list: diri_proto::methods::SessionListResult =
        serde_json::from_value(engine.call("session.list", json!({}))).expect("session list");

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(diri_ui::IconAssets),
        gpui_platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(|cx| crate::fonts::init(cx));
    let tokio = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime"),
    );
    let client = Arc::new(diri_client::DaemonClient::with_socket_path(&engine.socket));
    let runtime = Arc::new(crate::store::StoreRuntime::inert_with_client(client));
    runtime.store.write().unwrap().hydrate(list);
    let window = cx
        .open_window(size(px(1000.0), px(700.0)), {
            let runtime = Arc::clone(&runtime);
            let tokio = Arc::clone(&tokio);
            let id = id.clone();
            move |window, cx| {
                cx.new(|cx| {
                    let mut pane = TerminalPane::new_fixed(runtime, tokio, id, window, cx);
                    pane.set_viewport(
                        TerminalViewport {
                            x: 0.0,
                            y: 0.0,
                            width: 1000.0,
                            height: 700.0,
                        },
                        cx,
                    );
                    pane
                })
            }
        })
        .expect("window");

    let pump = |cx: &mut HeadlessAppContext| {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, _| window.present_if_needed())
            .expect("present");
    };
    let screen_text = |cx: &mut HeadlessAppContext| {
        cx.update_window(window.into(), |root, _, cx| {
            let pane = root.downcast::<TerminalPane>().unwrap();
            let pane = pane.read(cx);
            pane.residents
                .values()
                .map(|resident| {
                    let buffer = resident.element.buffer();
                    let buffer = buffer.read().unwrap();
                    buffer
                        .cells
                        .iter()
                        .filter_map(|cell| char::from_u32(cell.scalar))
                        .collect::<String>()
                })
                .collect::<String>()
        })
        .expect("screen")
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        pump(&mut cx);
        cx.update_window(window.into(), |root, _, cx| {
            let pane = root.downcast::<TerminalPane>().unwrap();
            pane.read(cx).claim_selected_control();
        })
        .expect("claim");
        if screen_text(&mut cx).contains("ready") {
            break;
        }
        assert!(Instant::now() < deadline, "the session never painted");
        std::thread::sleep(Duration::from_millis(5));
    }

    let type_key = |cx: &mut HeadlessAppContext, key: &str, wait: bool| {
        let before = latency_trace::finished_len();
        cx.update_window(window.into(), |root, window, cx| {
            let pane = root.downcast::<TerminalPane>().unwrap();
            pane.update(cx, |pane, cx| pane.handle_key_down(&typed(key), window, cx));
        })
        .expect("key");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            pump(cx);
            if !wait || latency_trace::finished_len() > before {
                return;
            }
            assert!(Instant::now() < deadline, "no echo frame for {key:?}");
            std::hint::spin_loop();
        }
    };
    let letters = "abcdefghijklmnopqrstuvwxyz";
    for index in 0..10 {
        type_key(&mut cx, &letters[index..=index], true);
        std::thread::sleep(Duration::from_millis(40));
    }
    latency_trace::drain();
    let mut on_line = 0;
    for index in 0..samples {
        if on_line >= 60 {
            type_key(&mut cx, "enter", false);
            let settle = Instant::now() + Duration::from_millis(150);
            while Instant::now() < settle {
                pump(&mut cx);
                std::thread::sleep(Duration::from_millis(2));
            }
            on_line = 0;
        }
        // Every third key on a line is a backspace, an echo that removes a
        // cell (counted per line: a fresh line has nothing to erase).
        let key = if on_line % 3 == 2 {
            "backspace".to_string()
        } else {
            letters[index % 26..=index % 26].to_string()
        };
        type_key(&mut cx, &key, true);
        on_line += 1;
        let pause = Instant::now() + Duration::from_millis(40);
        while Instant::now() < pause {
            pump(&mut cx);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let records = latency_trace::drain();
    println!("{}", latency_trace::render(&records));
    latency_trace::set_enabled(false);
    let _ = engine.call("session.kill", json!({ "sessionID": id.0 }));
    cx.update_window(window.into(), |_, window, _| window.remove_window())
        .expect("close");
    cx.run_until_parked();
}
