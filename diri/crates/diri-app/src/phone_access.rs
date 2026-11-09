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
        store: Arc<crate::store::StoreRuntime>,
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
            diri_web::start_phone_access(
            address,
            client,
            "Your Mac".into(),
            admission,
            Some(PhoneDesktop::start(store) as Arc<dyn diri_web::Desktop>),
        )
        .await?;
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

/// Ownership of the one in-flight [`PhoneAccess::start`]. Every Enable or
/// restart takes a fresh ticket and every stop (Turn off, the identity toggle,
/// a pairing-code restart) revokes it, so a start that finishes after its
/// rule changed is recognised as stale and its gateway dropped (which turns
/// it off) instead of being installed with the old admission rule.
#[derive(Debug, Default)]
pub struct PhoneStartSlot {
    next: u64,
    pending: Option<u64>,
}

/// One start's claim on the [`PhoneStartSlot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct PhoneStartTicket(u64);

impl PhoneStartSlot {
    /// Claims the slot for a new start, revoking any earlier one.
    pub fn begin(&mut self) -> PhoneStartTicket {
        self.next += 1;
        self.pending = Some(self.next);
        PhoneStartTicket(self.next)
    }

    /// Revokes the in-flight start, if any. Returns whether one was pending.
    pub fn cancel(&mut self) -> bool {
        self.pending.take().is_some()
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Settles `ticket`; `true` only when it still owns the slot, i.e. its
    /// result may be installed. A stale result must be dropped.
    pub fn finish(&mut self, ticket: PhoneStartTicket) -> bool {
        if self.pending == Some(ticket.0) {
            self.pending = None;
            true
        } else {
            false
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
    use super::PhoneStartSlot;

    #[test]
    fn a_start_revoked_while_in_flight_is_stale() {
        let mut slot = PhoneStartSlot::default();
        let first = slot.begin();
        assert!(slot.cancel(), "identity toggle revokes the pending start");
        assert!(
            !slot.finish(first),
            "the old rule's gateway must not install"
        );
        assert!(!slot.is_pending());
    }

    #[test]
    fn only_the_latest_start_may_install() {
        let mut slot = PhoneStartSlot::default();
        let first = slot.begin();
        let second = slot.begin();
        assert!(!slot.finish(first));
        assert!(slot.finish(second));
        assert!(!slot.finish(second), "a ticket settles once");
    }

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

/// What the phone gets from this app beyond the Engine: the sidebar order
/// this app draws (reorders applied as a drag here would be), and terminals
/// handed back to their pane after the phone resized them.
pub struct PhoneDesktop {
    runtime: Arc<crate::store::StoreRuntime>,
    changes: tokio::sync::watch::Sender<u64>,
    watcher: tokio::task::JoinHandle<()>,
}

impl PhoneDesktop {
    /// Starts watching the store; a phone is told whenever the drawn order
    /// moves. Must run inside the app's Tokio runtime.
    pub fn start(runtime: Arc<crate::store::StoreRuntime>) -> Arc<Self> {
        let (changes, _) = tokio::sync::watch::channel(0_u64);
        let mut store_changes = runtime.changes();
        let watched = Arc::clone(&runtime);
        let ticks = changes.clone();
        let watcher = tokio::spawn(async move {
            let mut last = layout(&watched);
            loop {
                match store_changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
                // Store changes arrive coalesced (at most every 50 ms), and the
                // projection is cached per store revision, so this is cheap.
                let next = layout(&watched);
                if next != last {
                    last = next;
                    ticks.send_modify(|tick| *tick = tick.wrapping_add(1));
                }
            }
        });
        Arc::new(Self {
            runtime,
            changes,
            watcher,
        })
    }
}

impl Drop for PhoneDesktop {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

fn layout(runtime: &crate::store::StoreRuntime) -> diri_web::SidebarLayout {
    let mut store = runtime.store.write().expect("session store lock poisoned");
    let projection = store.sidebar_projection();
    let prefs = store.preferences();
    diri_web::SidebarLayout {
        project_order: projection
            .projects
            .iter()
            .map(|group| group.project.id.0.clone())
            .collect(),
        session_order: projection
            .display_order
            .iter()
            .map(|id| id.0.clone())
            .collect(),
        pinned_sessions: prefs
            .sidebar_pinned_sessions
            .iter()
            .map(|id| id.0.clone())
            .collect(),
        pinned_projects: prefs
            .sidebar_pinned_projects
            .iter()
            .map(|id| id.0.clone())
            .collect(),
        reorderable: prefs.sidebar_ordering == crate::store::SidebarOrdering::Custom,
    }
}

impl diri_web::Desktop for PhoneDesktop {
    fn sidebar(&self) -> diri_web::SidebarLayout {
        layout(&self.runtime)
    }

    fn set_sidebar_order(
        &self,
        sessions: Option<Vec<String>>,
        projects: Option<Vec<String>>,
    ) -> Result<(), String> {
        {
            let mut store = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned");
            if store.preferences().sidebar_ordering != crate::store::SidebarOrdering::Custom {
                return Err("Your Mac sorts its sidebar by activity. Choose Custom order there to reorder.".into());
            }
            store
                .update_preferences(|prefs| {
                    if let Some(sessions) = sessions {
                        prefs.sidebar_session_order =
                            sessions.into_iter().map(diri_proto::SessionId).collect();
                    }
                    if let Some(projects) = projects {
                        prefs.sidebar_project_order =
                            projects.into_iter().map(diri_proto::ProjectId).collect();
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        // Redraw the Mac sidebar now, as its own drag would.
        self.runtime.publish_local_change();
        Ok(())
    }

    fn refit(&self, session: &str) {
        self.runtime
            .request_refit(diri_proto::SessionId::new(session));
    }

    fn sidebar_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;
    use diri_web::Desktop as _;

    #[tokio::test]
    async fn a_phone_reorder_lands_in_the_mac_prefs_only_when_sorting_by_hand() {
        let runtime = Arc::new(crate::store::StoreRuntime::inert());
        let sidebar = PhoneDesktop::start(Arc::clone(&runtime));
        assert!(sidebar.sidebar().reorderable);

        sidebar
            .set_sidebar_order(Some(vec!["s_b".into(), "s_a".into()]), None)
            .unwrap();
        {
            let store = runtime.store.read().unwrap();
            let prefs = store.preferences();
            assert_eq!(
                prefs.sidebar_session_order,
                [diri_proto::SessionId::new("s_b"), diri_proto::SessionId::new("s_a")]
            );
            assert!(prefs.sidebar_project_order.is_empty(), "untouched");
        }

        runtime
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.sidebar_ordering = crate::store::SidebarOrdering::NewestFirst;
            })
            .unwrap();
        assert!(!sidebar.sidebar().reorderable);
        assert!(
            sidebar
                .set_sidebar_order(None, Some(vec!["p_a".into()]))
                .is_err()
        );
        assert!(
            runtime
                .store
                .read()
                .unwrap()
                .preferences()
                .sidebar_project_order
                .is_empty()
        );
    }
}
