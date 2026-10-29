//! App-lifetime phone gateway; no service install or public bind.
//!
//! Two admission modes. By default a pairing token persists owner-only in
//! the app's support directory so phones stay paired across restarts;
//! [`PhoneAccess::reset_pairing`] rotates it. With "Identify devices with
//! Tailscale" on, there is no standing secret: each connection is identified
//! by the Mac's own tailscaled (WhoIs), only the owner's untagged devices
//! paired with a one-time code get in, and each can be revoked on its own.
use std::{
    net::Ipv4Addr,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

pub struct PhoneAccess {
    pub url: String,
    pub qr: qrcode::QrCode,
    address: Ipv4Addr,
    /// `Some` under tailnet admission.
    devices: Option<Arc<diri_web::Devices>>,
    server: tokio::task::JoinHandle<Result<(), String>>,
    awake: Option<Child>,
}

/// The store of devices paired under tailnet admission. One per app, shared
/// by Settings and the gateway so a revocation closes live connections.
pub fn paired_devices() -> Arc<diri_web::Devices> {
    Arc::new(diri_web::Devices::open(diri_web::phone_devices_path()))
}

impl PhoneAccess {
    pub async fn start(
        client: Arc<diri_client::DaemonClient>,
        tailnet: Option<Arc<diri_web::Devices>>,
    ) -> Result<Self, String> {
        let address = tailscale_address().await?;
        let admission = match &tailnet {
            None => diri_web::PhoneAdmission::Token(diri_web::phone_token_path()),
            Some(devices) => diri_web::PhoneAdmission::Tailnet {
                local: diri_web::LocalApi::discover()
                    .ok_or_else(|| TailscaleSetup::NotInstalled.message().to_owned())?,
                devices: Arc::clone(devices),
            },
        };
        let (url, server) =
            diri_web::start_phone_access(address, client, "Your Mac".into(), admission).await?;
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
                    return Err(crate::i18n::tf(
                        "settings.phone.awake_failed",
                        &[("error", &error)],
                    ));
                }
            }
        } else {
            None
        };
        Ok(Self {
            url,
            qr,
            address,
            devices: tailnet,
            server,
            awake,
        })
    }

    /// Whether this gateway admits by Tailscale identity and pairing.
    pub fn identifies_devices(&self) -> bool {
        self.devices.is_some()
    }

    /// Replaces the one-time pairing code shown in the QR (tailnet admission
    /// only). Paired devices are unaffected.
    pub fn new_pairing_code(&mut self) -> Result<(), String> {
        let Some(devices) = &self.devices else {
            return Ok(());
        };
        let code = devices.mint_code().map_err(|error| error.to_string())?;
        let url = diri_web::phone_link(self.address, &code);
        self.qr = qrcode::QrCode::new(url.as_bytes()).map_err(|error| error.to_string())?;
        self.url = url;
        Ok(())
    }

    /// Replaces the pairing code: every paired phone must scan the new one.
    /// Call with access off; [`Self::start`] then serves the new code.
    pub fn reset_pairing() -> Result<(), String> {
        diri_web::reset_phone_pairing(&diri_web::phone_token_path())
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
    match check_tailscale().await {
        TailscaleSetup::Ready(address) => Ok(address),
        state => Err(state.message().into()),
    }
}

/// Sanitized setup facts only. Never surface status JSON, login URLs or profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailscaleSetup {
    NotInstalled,
    NeedsLogin,
    NeedsApproval,
    Stopped,
    Unavailable,
    Ready(Ipv4Addr),
}

impl TailscaleSetup {
    pub fn message(self) -> &'static str {
        crate::i18n::t(match self {
            Self::NotInstalled => "settings.phone.tailscale_not_installed",
            Self::NeedsLogin => "settings.phone.tailscale_needs_login",
            Self::NeedsApproval => "settings.phone.tailscale_needs_approval",
            Self::Stopped => "settings.phone.tailscale_stopped",
            Self::Unavailable => "settings.phone.tailscale_unavailable",
            Self::Ready(_) => "settings.phone.tailscale_ready",
        })
    }
}

pub async fn check_tailscale() -> TailscaleSetup {
    let mut result = TailscaleSetup::NotInstalled;
    for executable in [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/usr/bin/tailscale",
    ] {
        if !std::path::Path::new(executable).is_file() {
            continue;
        }
        result = TailscaleSetup::Unavailable;
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
            && let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        {
            // `status` may exit nonzero while still reporting NeedsLogin.
            result = setup_from_status(&status);
            if result != TailscaleSetup::Unavailable {
                return result;
            }
        }
    }
    result
}

fn setup_from_status(status: &serde_json::Value) -> TailscaleSetup {
    match status["BackendState"].as_str() {
        Some("NeedsLogin") => TailscaleSetup::NeedsLogin,
        Some("NeedsMachineAuth") => TailscaleSetup::NeedsApproval,
        Some("Stopped") => TailscaleSetup::Stopped,
        Some("Running") => address_from_status(status)
            .map(TailscaleSetup::Ready)
            .unwrap_or(TailscaleSetup::Unavailable),
        _ => TailscaleSetup::Unavailable,
    }
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
    fn setup_distinguishes_actionable_states_without_exposing_status_data() {
        for (state, expected) in [
            ("NeedsLogin", TailscaleSetup::NeedsLogin),
            ("NeedsMachineAuth", TailscaleSetup::NeedsApproval),
            ("Stopped", TailscaleSetup::Stopped),
            ("Starting", TailscaleSetup::Unavailable),
            ("Running", TailscaleSetup::Unavailable),
        ] {
            assert_eq!(
                setup_from_status(&serde_json::json!({
                    "BackendState": state, "AuthURL": "secret", "TailscaleIPs": ["192.168.1.2"]
                })),
                expected
            );
            assert!(!expected.message().contains("secret"));
        }
        assert_eq!(
            setup_from_status(&serde_json::json!({
                "BackendState": "Running", "TailscaleIPs": ["100.90.0.2"]
            })),
            TailscaleSetup::Ready(Ipv4Addr::new(100, 90, 0, 2))
        );
        assert_eq!(
            setup_from_status(&serde_json::json!({})),
            TailscaleSetup::Unavailable
        );
    }
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
