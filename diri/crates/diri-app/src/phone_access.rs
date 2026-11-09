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
            Some(PhoneSidebar::start(store) as Arc<dyn diri_web::Desktop>),
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

/// The Mac sidebar, as the phone sees it: the order this app draws, and
/// moves from the phone applied exactly as a drag here would.
pub struct PhoneSidebar {
    runtime: Arc<crate::store::StoreRuntime>,
    /// The last computed layout. Phone reads answer from here so they never
    /// wait on the store lock the UI thread also holds.
    cached: std::sync::Mutex<Arc<diri_web::SidebarLayout>>,
    changes: tokio::sync::watch::Sender<u64>,
    watcher: std::sync::OnceLock<tokio::task::JoinHandle<()>>,
}

impl PhoneSidebar {
    /// Starts watching the store; a phone is told whenever the drawn order
    /// moves. Must run inside the app's Tokio runtime.
    pub fn start(runtime: Arc<crate::store::StoreRuntime>) -> Arc<Self> {
        let (changes, _) = tokio::sync::watch::channel(0_u64);
        let mut store_changes = runtime.changes();
        let sidebar = Arc::new(Self {
            cached: std::sync::Mutex::new(Arc::new(layout(&runtime))),
            runtime,
            changes,
            watcher: std::sync::OnceLock::new(),
        });
        let weak = Arc::downgrade(&sidebar);
        let watcher = tokio::spawn(async move {
            loop {
                match store_changes.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
                let Some(sidebar) = weak.upgrade() else { return };
                // Store changes arrive coalesced (at most every 50 ms) and the
                // projection is cached per store revision; the lock is still
                // the UI's, so take it on a blocking thread.
                let runtime = Arc::clone(&sidebar.runtime);
                let Ok(next) = tokio::task::spawn_blocking(move || layout(&runtime)).await else {
                    continue;
                };
                sidebar.publish(next);
            }
        });
        let _ = sidebar.watcher.set(watcher);
        sidebar
    }

    /// Caches `next` and ticks the phone if it differs from what it last saw.
    fn publish(&self, next: diri_web::SidebarLayout) {
        let mut cached = self.cached.lock().expect("sidebar cache poisoned");
        if **cached != next {
            *cached = Arc::new(next);
            drop(cached);
            self.changes
                .send_modify(|tick| *tick = tick.wrapping_add(1));
        }
    }
}

impl Drop for PhoneSidebar {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.get() {
            watcher.abort();
        }
    }
}

fn layout(runtime: &crate::store::StoreRuntime) -> diri_web::SidebarLayout {
    let mut store = runtime.store.write().expect("session store lock poisoned");
    layout_of(&mut store)
}

fn layout_of(store: &mut crate::store::SessionStore) -> diri_web::SidebarLayout {
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

/// Where [`place_in`] puts a row, by the ids of the rows around it.
enum Place<'a, T> {
    Before(&'a T),
    After(&'a T),
    End,
}

/// Moves `moved` within the saved `order`, as the Mac's drag helpers do.
/// Every row named must be in the order: a phone naming a row the Mac no
/// longer has is working from a stale layout, and nothing moves.
fn place_in<T: PartialEq>(
    order: &mut Vec<T>,
    moved: &T,
    place: Place<'_, T>,
) -> Result<(), diri_web::SidebarRefusal> {
    let stale = || {
        diri_web::SidebarRefusal::Conflict(
            "That row changed on your Mac. Refresh and try again.".into(),
        )
    };
    let from = order.iter().position(|id| id == moved).ok_or_else(stale)?;
    if let Place::Before(anchor) | Place::After(anchor) = &place {
        if *anchor == moved {
            return Ok(());
        }
        if !order.contains(*anchor) {
            return Err(stale());
        }
    }
    let item = order.remove(from);
    let position = |anchor: &T| order.iter().position(|id| id == anchor).unwrap_or(order.len());
    let to = match place {
        Place::Before(anchor) => position(anchor),
        Place::After(anchor) => position(anchor) + 1,
        Place::End => order.len(),
    };
    order.insert(to, item);
    Ok(())
}

impl diri_web::Desktop for PhoneSidebar {
    fn sidebar(&self) -> diri_web::SidebarLayout {
        (**self.cached.lock().expect("sidebar cache poisoned")).clone()
    }

    fn move_sidebar_row(&self, row: diri_web::SidebarMove) -> Result<(), diri_web::SidebarRefusal> {
        use diri_web::{SidebarPlace, SidebarRefusal, SidebarRow};
        let failed = |error: std::io::Error| SidebarRefusal::Failed(error.to_string());
        let next = {
            let mut store = self
                .runtime
                .store
                .write()
                .expect("session store lock poisoned");
            if store.preferences().sidebar_ordering != crate::store::SidebarOrdering::Custom {
                return Err(SidebarRefusal::Conflict(
                    "Your Mac sorts its sidebar by activity. Choose Custom order there to reorder."
                        .into(),
                ));
            }
            // The saved manual order, reconciled to every live row — the
            // same list a drag on the Mac edits — not the drawn order.
            match row.row {
                SidebarRow::Session => {
                    let wrap = |id: &String| diri_proto::SessionId(id.clone());
                    let mut order = store.sidebar_session_order();
                    let anchor = match &row.place {
                        SidebarPlace::Before(id) | SidebarPlace::After(id) => Some(wrap(id)),
                        SidebarPlace::End => None,
                    };
                    let place = match (&row.place, &anchor) {
                        (SidebarPlace::Before(_), Some(anchor)) => Place::Before(anchor),
                        (SidebarPlace::After(_), Some(anchor)) => Place::After(anchor),
                        _ => Place::End,
                    };
                    place_in(&mut order, &wrap(&row.id), place)?;
                    store.set_session_order(order).map_err(failed)?;
                }
                SidebarRow::Project => {
                    let wrap = |id: &String| diri_proto::ProjectId(id.clone());
                    let mut order = store.sidebar_project_order();
                    let anchor = match &row.place {
                        SidebarPlace::Before(id) | SidebarPlace::After(id) => Some(wrap(id)),
                        SidebarPlace::End => None,
                    };
                    let place = match (&row.place, &anchor) {
                        (SidebarPlace::Before(_), Some(anchor)) => Place::Before(anchor),
                        (SidebarPlace::After(_), Some(anchor)) => Place::After(anchor),
                        _ => Place::End,
                    };
                    place_in(&mut order, &wrap(&row.id), place)?;
                    store.set_project_order(order).map_err(failed)?;
                }
            }
            layout_of(&mut store)
        };
        // The phone refetches as soon as this returns: it must see the move.
        self.publish(next);
        // Redraw the Mac sidebar now, as its own drag would.
        self.runtime.publish_local_change();
        Ok(())
    }

    fn sidebar_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;
    use diri_web::{Desktop as _, SidebarMove, SidebarPlace, SidebarRefusal, SidebarRow};

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn a_move_edits_only_the_moved_row_in_the_saved_order() {
        let mut order = ids(&["a", "b", "c", "d"]);
        let id = |value: &str| value.to_owned();
        place_in(&mut order, &id("d"), Place::Before(&id("b"))).unwrap();
        assert_eq!(order, ids(&["a", "d", "b", "c"]));
        place_in(&mut order, &id("a"), Place::After(&id("b"))).unwrap();
        assert_eq!(order, ids(&["d", "b", "a", "c"]));
        place_in(&mut order, &id("d"), Place::End).unwrap();
        assert_eq!(order, ids(&["b", "a", "c", "d"]));
        place_in(&mut order, &id("c"), Place::Before(&id("c"))).unwrap();
        assert_eq!(order, ids(&["b", "a", "c", "d"]), "onto itself");

        // Rows the Mac no longer has: nothing moves.
        for (moved, anchor) in [("gone", "a"), ("a", "gone")] {
            assert!(matches!(
                place_in(&mut order, &id(moved), Place::Before(&id(anchor))),
                Err(SidebarRefusal::Conflict(_))
            ));
            assert_eq!(order, ids(&["b", "a", "c", "d"]));
        }
    }

    #[tokio::test]
    async fn a_phone_move_lands_in_the_mac_prefs_only_when_sorting_by_hand() {
        let runtime = Arc::new(crate::store::StoreRuntime::inert());
        let sidebar = PhoneSidebar::start(Arc::clone(&runtime));
        assert!(sidebar.sidebar().reorderable);

        // A row the Mac does not have is a stale phone, not a server failure.
        assert!(matches!(
            sidebar.move_sidebar_row(SidebarMove {
                row: SidebarRow::Session,
                id: "s_gone".into(),
                place: SidebarPlace::End,
            }),
            Err(SidebarRefusal::Conflict(_))
        ));

        runtime
            .store
            .write()
            .unwrap()
            .update_preferences(|prefs| {
                prefs.sidebar_ordering = crate::store::SidebarOrdering::NewestFirst;
            })
            .unwrap();
        let refused = sidebar.move_sidebar_row(SidebarMove {
            row: SidebarRow::Project,
            id: "p_b".into(),
            place: SidebarPlace::Before("p_a".into()),
        });
        assert!(matches!(refused, Err(SidebarRefusal::Conflict(_))), "{refused:?}");
    }
}
