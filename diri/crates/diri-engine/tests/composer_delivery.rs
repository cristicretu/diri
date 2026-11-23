//! Follow-ups and echo probes through the desktop binary attach protocol.
//! The fixture keeps painting its progress while the PTY accepts input. It
//! records submitted lines independently of the terminal's visible echo.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diri_engine::{ManifestEngine, Registry, control::ControlServer};
use diri_proto::ControlMessage;
#[path = "support/composer.rs"]
mod composer;
use composer::Composer;
use serde_json::json;

// Canonical mode lets the kernel implement echo and line editing. Preserve
// the bracketed-paste delimiters so the test can inspect the exact PTY input.
// A cursor-saving progress row changes independently of the composer.
const COMPOSER: &str = r#"
stty echo echoe echok
printf '\033[2J\033[2H\033[?2004h'
(
    i=0
    while [ ! -f finished ]; do
        printf '\0337\033[1;1Hworking %s\0338' "$i"
        i=$((i+1))
        sleep 0.05
    done
) &
progress=$!
n=0
while :; do
    printf 'compose> '
    IFS= read -r message || break
    [ "$message" != finish ] || break
    printf '%s\n' "$message" >> submitted
    n=$((n+1))
    printf 'accepted %s\n' "$n"
done
touch finished
wait "$progress"
printf 'complete\n'
"#;

fn serve(server: &Arc<ControlServer>) -> (UnixStream, std::thread::JoinHandle<()>) {
    let (client, engine) = UnixStream::pair().unwrap();
    let server = Arc::clone(server);
    let worker = std::thread::spawn(move || server.serve(engine).unwrap());
    (client, worker)
}

fn wait_for(composer: &mut Composer, needle: &str, timeout: Duration) {
    assert!(
        composer.wait(timeout, |rows| rows.iter().any(|r| r.contains(needle))),
        "missing grid echo of {needle:?}"
    );
}

#[test]
fn followups_arrive_once_and_probes_remain_editable_during_work() {
    let root = tempfile::tempdir().unwrap();
    let manifests = ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
        .unwrap()
        .0;
    let registry = Arc::new(Mutex::new(Registry::new(
        Arc::new(manifests),
        root.path().join("state.json"),
    )));
    let server = Arc::new(ControlServer::new(
        registry,
        root.path().join("engine.sock"),
    ));
    let (mut control, control_worker) = serve(&server);
    let spawn = ControlMessage::Request {
        id: 1,
        method: "session.spawn".into(),
        params: Some(
            json!({"kind": {"shell": {}}, "cwd": root.path(), "argv": ["/bin/sh", "-c", COMPOSER]}),
        ),
    };
    writeln!(control, "{}", serde_json::to_string(&spawn).unwrap()).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(&control).read_line(&mut line).unwrap();
    let ControlMessage::Response {
        result: Ok(record), ..
    } = serde_json::from_str(&line).unwrap()
    else {
        panic!("session.spawn: {line}");
    };
    let (data, attach_worker) = serve(&server);
    let mut composer = Composer::attach(data, record["id"].as_str().unwrap());
    wait_for(&mut composer, "compose> ", Duration::from_secs(10));

    let followups = [
        "first: keep the label",
        "second: preserve spacing",
        "third: keep punctuation!",
    ];
    for (i, (probe, followup)) in ["x", "y", "z"].into_iter().zip(followups).enumerate() {
        composer.key(probe, false);
        wait_for(
            &mut composer,
            &format!("compose> {probe}"),
            Duration::from_secs(2),
        );
        composer.key("u", true);
        composer.paste(followup);
        composer.key("m", true);
        wait_for(
            &mut composer,
            &format!("accepted {}", i + 1),
            Duration::from_secs(5),
        );
    }
    for ch in ["f", "i", "n", "i", "s", "h"] {
        composer.key(ch, false);
    }
    composer.key("m", true);
    wait_for(&mut composer, "complete", Duration::from_secs(5));

    let submitted = std::fs::read(root.path().join("submitted")).unwrap();
    let expected = followups
        .map(|text| format!("\x1b[200~{text}\x1b[201~\n"))
        .concat()
        .into_bytes();
    assert_eq!(
        submitted, expected,
        "submitted bytes, including order, multiplicity and cleared probes"
    );
    drop(composer);
    drop(control);
    attach_worker.join().unwrap();
    control_worker.join().unwrap();
}
