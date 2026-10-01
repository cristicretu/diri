//! Native Windows Holder lifecycle through the production Session/IPC path.
//! Uses disposable PowerShell processes, no SSH host or installed Agent.
#![cfg(windows)]

use diri_engine::holder::{HolderClient, HolderPaths};
use diri_engine::session::{HolderConfig, Session, SessionSpec};
use diri_engine::{Authority, ManifestEngine, PtySpec};
use diri_platform::windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_TERMINATE, TerminateProcess,
};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn engine() -> Arc<ManifestEngine> {
    Arc::new(
        ManifestEngine::load_dir(&diri_engine::detect::bundled_manifest_dir())
            .unwrap()
            .0,
    )
}

fn spec(root: &Path, holder: &HolderConfig, script: &str) -> SessionSpec {
    let mut pty = PtySpec::new(
        vec![
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-Command".into(),
            script.into(),
        ],
        root,
    )
    .size(80, 24);
    pty.env = diri_platform::launch::local_environment();
    pty.env.push(("TERM".into(), "xterm-256color".into()));
    SessionSpec {
        id: "s_windows".into(),
        pty,
        manifest_id: "shell".into(),
        authority: Authority::ProcessOnly,
        logs_dir: root.join("logs"),
        holder: Some(holder.clone()),
        remote: None,
        defer_launch: false,
    }
}

/// Even an assertion failure must stop this test's detached Holder and Job.
struct Cleanup {
    client: HolderClient,
    holder: OwnedHandle,
}
impl Cleanup {
    fn new(holder: &HolderConfig) -> Self {
        let paths = HolderPaths::new(&holder.holders_dir, "s_windows");
        let pid: u32 = std::fs::read_to_string(paths.pid_file())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: capture the handle while our new Holder is known alive;
        // subsequent cleanup uses the handle, never a potentially reused PID.
        let raw = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        assert!(
            !raw.is_null(),
            "open test Holder: {}",
            std::io::Error::last_os_error()
        );
        Self {
            client: HolderClient::new(paths.socket()),
            holder: unsafe { OwnedHandle::from_raw_handle(raw) },
        }
    }
    fn kill_holder(&self) {
        // SAFETY: this owned handle identifies only the Holder this test created.
        assert_ne!(
            unsafe { TerminateProcess(self.holder.as_raw_handle(), 77) },
            0
        );
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.client.kill_tree();
        // SAFETY: the same captured test Holder; harmless if already exited.
        unsafe {
            TerminateProcess(self.holder.as_raw_handle(), 77);
        }
    }
}

#[test]
fn a_native_session_is_adopted_with_the_same_identity_screen_and_input() {
    let root = tempfile::tempdir().unwrap();
    let holder = HolderConfig {
        holders_dir: root.path().join("holders"),
        executable: env!("CARGO_BIN_EXE_diri-holder").into(),
    };
    let engine = engine();
    let script = "Write-Output 'ready'; while ($null -ne ($line = [Console]::ReadLine())) { Write-Output ('seen:' + $line) }";
    let session = Session::spawn(spec(root.path(), &holder, script), Arc::clone(&engine)).unwrap();
    let cleanup = Cleanup::new(&holder);
    wait_until("PowerShell ready", || {
        session.screen_lines().join("\n").contains("ready")
    });
    session.write_input(b"before\r").unwrap();
    wait_until("input evaluated", || {
        session.screen_lines().join("\n").contains("seen:before")
    });
    session.resize(100, 30).unwrap();
    let before = cleanup.client.stat().unwrap();
    let identity = before
        .verified_child_identity()
        .expect("live native identity");
    assert_eq!((before.cols, before.rows), (Some(100), Some(30)));
    drop(session); // The Engine's Session/follower disappear, not the PTY owner.

    let stat = cleanup.client.stat().expect("Holder survives detach");
    assert_eq!(stat.verified_child_identity(), Some(identity));
    let mut adopted =
        Session::adopt(spec(root.path(), &holder, script), &holder, &stat, engine).unwrap();
    wait_until("restored screen", || {
        adopted.screen_lines().join("\n").contains("seen:before")
    });
    adopted.write_input(b"after\r").unwrap();
    wait_until("input after adoption", || {
        adopted.screen_lines().join("\n").contains("seen:after")
    });
    assert_eq!(
        cleanup.client.stat().unwrap().verified_child_identity(),
        Some(identity)
    );
    adopted.terminate(Duration::from_secs(5)).unwrap();
    wait_until("session cleanup", || !cleanup.client.is_alive());
}

#[test]
fn holder_failure_kills_the_native_job_including_a_grandchild() {
    let root = tempfile::tempdir().unwrap();
    let holder = HolderConfig {
        holders_dir: root.path().join("holders"),
        executable: env!("CARGO_BIN_EXE_diri-holder").into(),
    };
    let script = "$child = Start-Process powershell.exe -ArgumentList '-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 120' -PassThru; Write-Output 'tree-ready'; Start-Sleep -Seconds 120";
    let session = Session::spawn(spec(root.path(), &holder, script), engine()).unwrap();
    let cleanup = Cleanup::new(&holder);
    wait_until("child tree ready", || {
        session.screen_lines().join("\n").contains("tree-ready")
    });
    let leader = cleanup.client.stat().unwrap().child_pid;
    let mut identities = Vec::new();
    wait_until("live child and grandchild identities", || {
        identities = diri_engine::holder::process_tree::enumerate(leader)
            .into_iter()
            .filter_map(|sample| diri_pty::process_identity::observe(sample.pid as u32).ok())
            .collect();
        identities.len() >= 2
    });
    assert!(diri_engine::holder::process_tree::has_children(leader));
    cleanup.kill_holder();
    wait_until("Job tree killed on Holder failure", || {
        identities.iter().all(|identity| {
            diri_pty::process_identity::observe(identity.pid())
                .ok()
                .as_ref()
                != Some(identity)
        })
    });
    drop(session);
}
