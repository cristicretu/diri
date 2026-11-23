//! Process spawn to the client's verified Hello, without a desktop window.
//! Pass the path to a sibling-workspace dirijord-rs. Every run uses a fresh home.
use std::error::Error;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diri_client::DaemonClient;
use diri_proto::paths::{ENV_APP_SUPPORT, SOCKET_FILE_NAME};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let engine = std::env::args_os()
        .nth(1)
        .ok_or("expected dirijord-rs path")?;
    for _ in 0..7 {
        let home = tempfile::tempdir()?;
        let state = home.path().join("state");
        let client = DaemonClient::with_socket_path(state.join(SOCKET_FILE_NAME));
        let mut command = Command::new(&engine);
        command
            .env_clear()
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env(ENV_APP_SUPPORT, &state)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("DIRI_TELEMETRY", "off")
            .env("DIRI_MCP_TRANSPORT", "stdio")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let start = Instant::now();
        let mut child = command.spawn()?;
        client.connect();
        let ready = client.wait_until_connected(Duration::from_secs(15)).await;
        let elapsed = start.elapsed();
        client.shutdown().await;
        let _ = child.kill();
        child.wait()?;
        ready?;
        println!("{:.2}", elapsed.as_secs_f64() * 1000.0);
    }
    Ok(())
}
