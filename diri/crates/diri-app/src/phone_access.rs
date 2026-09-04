//! App-lifetime phone gateway; no service install, public bind, or credentials on disk.
use std::{
    net::Ipv4Addr,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

pub struct PhoneAccess {
    pub url: String,
    pub qr: qrcode::QrCode,
    server: tokio::task::JoinHandle<Result<(), String>>,
    awake: Option<Child>,
}

impl PhoneAccess {
    pub async fn start(client: Arc<diri_client::DaemonClient>) -> Result<Self, String> {
        let address = tailscale_address().await?;
        let (url, server) =
            diri_web::start_phone_access(address, client, "Your Mac".into()).await?;
        let qr = match qrcode::QrCode::new(url.as_bytes()) {
            Ok(qr) => qr,
            Err(error) => {
                server.abort();
                return Err(error.to_string());
            }
        };
        // -i prevents idle sleep, not deliberate sleep or closing a laptop lid.
        // -w also guarantees cleanup if the app crashes.
        let awake = if cfg!(target_os = "macos") {
            match Command::new("/usr/bin/caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => Some(child),
                Err(error) => {
                    server.abort();
                    return Err(format!("Cannot keep this Mac awake: {error}"));
                }
            }
        } else {
            None
        };
        Ok(Self {
            url,
            qr,
            server,
            awake,
        })
    }

    pub fn is_running(&self) -> bool {
        !self.server.is_finished()
    }
}

impl Drop for PhoneAccess {
    fn drop(&mut self) {
        self.server.abort();
        if let Some(child) = &mut self.awake {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn tailscale_address() -> Result<Ipv4Addr, String> {
    for executable in [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/usr/bin/tailscale",
    ] {
        if !std::path::Path::new(executable).is_file() {
            continue;
        }
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(executable)
                .args(["status", "--json"])
                // The macOS bundle otherwise guesses GUI mode when launched
                // by a GUI app. See tailscale.com/docs/reference/tailscale-cli.
                .env("TAILSCALE_BE_CLI", "1")
                .kill_on_drop(true)
                .stdin(Stdio::null())
                .output(),
        )
        .await;
        if let Ok(Ok(output)) = output
            && output.status.success()
            && let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            && let Some(address) = address_from_status(&status)
        {
            return Ok(address);
        }
    }
    Err("Open Tailscale on this Mac and sign in. Then connect Tailscale on your iPhone with the same account and try again.".into())
}

fn address_from_status(status: &serde_json::Value) -> Option<Ipv4Addr> {
    if status["BackendState"].as_str() != Some("Running") {
        return None;
    }
    status["TailscaleIPs"]
        .as_array()?
        .iter()
        .filter_map(|ip| ip.as_str()?.parse::<Ipv4Addr>().ok())
        .find(|ip| {
            let [a, b, _, _] = ip.octets();
            a == 100 && (64..=127).contains(&b)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_a_running_private_tailnet_is_eligible() {
        for state in ["Stopped", "NeedsLogin"] {
            assert!(
                address_from_status(
                    &serde_json::json!({"BackendState": state, "TailscaleIPs": ["100.90.0.2"]})
                )
                .is_none()
            );
        }
        assert!(address_from_status(&serde_json::json!({"BackendState":"Running", "TailscaleIPs":["192.168.1.4","8.8.8.8"]})).is_none());
        assert_eq!(
            address_from_status(
                &serde_json::json!({"BackendState":"Running", "TailscaleIPs":["fd7a::1","100.90.0.2"]})
            ),
            Some(Ipv4Addr::new(100, 90, 0, 2))
        );
    }
}
