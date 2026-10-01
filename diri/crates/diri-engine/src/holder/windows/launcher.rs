//! One detached Windows Holder per session. No Engine-owned ConPTY/Job handle
//! crosses the launch boundary, so Engine replacement cannot kill the Agent.
use super::{HolderClient, HolderError, HolderLaunchSpec, HolderPaths, HolderResult};
use diri_platform::windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
};
use std::os::windows::process::CommandExt;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct HolderLauncher;
impl HolderLauncher {
    pub fn default_executable_path() -> PathBuf {
        std::env::var_os("DIRIJOR_HOLDER_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("diri.exe"))
                    .with_file_name("diri-holder.exe")
            })
    }
    pub fn launch(
        executable: &Path,
        paths: &HolderPaths,
        spec: &HolderLaunchSpec,
    ) -> HolderResult<i32> {
        diri_platform::security::private_dir_all(&paths.directory)
            .map_err(|e| HolderError::io("private Holder directory", e))?;
        let lock = diri_platform::security::open_private_rw(
            &paths
                .directory
                .join(format!("{}.launch.lock", paths.session_id)),
        )
        .map_err(|e| HolderError::io("Holder launch lock", e))?;
        lock.lock()
            .map_err(|e| HolderError::io("lock Holder launch", e))?;
        let client = HolderClient::new(paths.socket());
        if client.is_alive() {
            return holder_pid(paths);
        }
        let spec_path = paths.directory.join(format!(
            "{}.launch-{}.json",
            paths.session_id,
            crate::inject::uuid_v4()
        ));
        let mut file = diri_platform::security::create_private(&spec_path)
            .map_err(|e| HolderError::io("create launch spec", e))?;
        let mut bytes = serde_json::to_vec(spec).map_err(|e| HolderError::Launch(e.to_string()))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| HolderError::io("write launch spec", e))?;
        drop(file);
        let mut command = Command::new(executable);
        command
            .arg("--spec")
            .arg(&spec_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // An enclosing launcher Job must allow breakaway. Refusal is explicit:
        // silently inheriting it would violate session survival.
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_file(&spec_path);
                return Err(HolderError::io("spawn detached Windows Holder", e));
            }
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if client.is_alive() {
                return Ok(child.id() as i32);
            }
            if child
                .try_wait()
                .map_err(|e| HolderError::io("Holder startup", e))?
                .is_some()
            {
                return Err(HolderError::Launch(
                    "Windows Holder exited during startup".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // An ambiguous timeout may have launched an Agent. Preserve its spec and
        // process for explicit recovery; never duplicate or kill it by guess.
        Err(HolderError::Launch(
            "Windows Holder startup timed out; inspect the existing session before retrying".into(),
        ))
    }
}
fn holder_pid(paths: &HolderPaths) -> HolderResult<i32> {
    std::fs::read_to_string(paths.pid_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| HolderError::Launch("live Holder has no valid PID record".into()))
}
