//! What a remote session costs, with the network taken out of the question.
//!
//! A remote session is a different machine from a local one: the helper runs
//! the emulator on the far side and ships grid updates, and the engine keeps a
//! mirror rather than parsing anything itself. None of the local pipeline's
//! tuning applies to it, and until now none of its measurements did either.
//!
//! SSH is replaced by a shim that runs what it is handed as a local process,
//! so what is measured is the protocol and both ends of it — the part that is
//! the same whatever the link — rather than somebody's network. A real link
//! adds its own latency on top; it does not subtract any of this.
//!
//! Usage: remotebench <payload> [rounds]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diri_engine::remote::binding::RemoteBindingStore;
use diri_engine::remote::executor::ProcessExecutor;
use diri_engine::remote::manager::{ArtifactCatalog, RemoteManager};
use diri_engine::session::{RemoteSessionSpec, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_proto::hosts::HostEntry;
use diri_proto::remote_pty::{LaunchRequest, PersistenceCapability, SessionToken};

/// Stands in for ssh: runs the command it is handed on this machine, with a
/// HOME of our choosing so the helper install lands inside the sandbox.
fn write_ssh_shim(dir: &Path, home: &Path) -> PathBuf {
    let path = dir.join("ssh-shim");
    let mut file = std::fs::File::create(&path).expect("create ssh shim");
    // ssh is invoked as `ssh <options> -- <destination> <command>`, so the
    // command to run is always the last argument.
    write!(
        file,
        "#!/bin/sh\nexport HOME='{home}'\neval \"command=\\${{$#}}\"\nexec /bin/sh -c \"$command\"\n",
        home = home.display()
    )
    .expect("write ssh shim");
    drop(file);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn host() -> HostEntry {
    // The destination is never resolved: the shim ignores it.
    serde_json::from_value(serde_json::json!({
        "id": "bench",
        "ssh": "bench@localhost",
    }))
    .expect("host entry")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let payload = args.next().expect("usage: remotebench <payload> [rounds]");
    let rounds: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);
    let bytes = std::fs::metadata(&payload).expect("payload").len();

    let root = std::env::temp_dir().join(format!("diri-remotebench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".cache")).expect("home");
    std::fs::create_dir_all(root.join("bin")).expect("bin");
    let shim = write_ssh_shim(&root.join("bin"), &home);

    let helper = PathBuf::from(
        std::env::var("DIRI_REMOTE_BIN").expect("set DIRI_REMOTE_BIN to a built diri-remote"),
    );
    let catalog = ArtifactCatalog::from_native_helper(&helper).expect("helper catalog");
    let manager = Arc::new(
        RemoteManager::new(ProcessExecutor::new(&shim), catalog, root.join("control"))
            .expect("remote manager"),
    );

    let installed = Instant::now();
    let helper = manager.ensure_helper(&host()).expect("install helper");
    println!(
        "helper installed in {:.0} ms",
        installed.elapsed().as_secs_f64() * 1000.0
    );

    let manifests = diri_engine::detect::bundled_manifest_dir()
        .canonicalize()
        .expect("manifests");
    let (engine, _) = ManifestEngine::load_dir(&manifests).expect("load");
    let engine = Arc::new(engine);

    for round in 0..rounds {
        let id = format!("s_remote_{round}");
        let token = SessionToken::new(format!("token-{round}-0123456789abcdef")).expect("token");
        let spec = SessionSpec {
            id: id.clone(),
            pty: PtySpec::new(vec!["/bin/sh".into()], "/tmp").size(153, 39),
            manifest_id: "shell".into(),
            authority: Authority::ProcessOnly,
            logs_dir: root.join("logs"),
            holder: None,
            remote: Some(RemoteSessionSpec {
                manager: Arc::clone(&manager),
                helper: helper.clone(),
                launch: LaunchRequest {
                    session_id: id.clone(),
                    session_token: token,
                    argv: vec!["/bin/sh".into(), "-c".into(), format!("cat {payload}")],
                    cwd: "/tmp".into(),
                    environment: Vec::new(),
                    cols: 153,
                    rows: 39,
                    persistence: PersistenceCapability::NonPersistent,
                },
                host_id: "bench".into(),
                binding_store: RemoteBindingStore::new(root.join("bindings"))
                    .expect("binding store"),
            }),
            defer_launch: false,
        };

        let started = Instant::now();
        let session = Session::spawn(spec, Arc::clone(&engine)).expect("spawn remote");
        let deadline = Instant::now() + Duration::from_secs(300);
        while !session.view().exited && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let elapsed = started.elapsed().as_secs_f64();
        let mb = bytes as f64 / (1 << 20) as f64;
        println!(
            "round {round}: {mb:.1} MB in {:.0} ms = {:.1} MB/s{}",
            elapsed * 1000.0,
            mb / elapsed,
            if session.view().exited {
                ""
            } else {
                "  (TIMED OUT)"
            }
        );
    }

    let _ = manager.close_control_masters();
    let _ = std::fs::remove_dir_all(&root);
}
