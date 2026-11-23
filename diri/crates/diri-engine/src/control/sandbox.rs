//! `sandbox.*` on the control socket: the Ghost Desktop pool, created on
//! first use so an Engine that never runs one pays nothing.

use std::sync::Arc;

use diri_proto::sandbox::{
    SandboxActParams, SandboxFrameParams, SandboxListParams, SandboxSnapshotParams,
    SandboxStartParams, SandboxStopParams,
};
use diri_proto::{ControlError, JsonValue, Method};

use super::{ControlServer, decode, encode, poisoned};
use crate::sandbox::{Caller, SandboxError, SandboxPool};

fn control_error(error: SandboxError) -> ControlError {
    ControlError::new(error.code, error.message)
}

impl ControlServer {
    pub(super) fn sandbox_pool(&self) -> &SandboxPool {
        self.sandbox.get_or_init(|| {
            let pool = SandboxPool::for_engine(&self.logs_dir);
            let registry = Arc::clone(&self.registry);
            // Called with no pool lock held; takes the Registry briefly.
            pool.set_liveness(Arc::new(move |id: &str| {
                let registry = registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                registry
                    .record(id)
                    .is_some_and(|record| !record.is_archived() && registry.is_live(id))
            }));
            pool
        })
    }

    /// A session the call names must exist; the pool checks it is live.
    fn sandbox_caller_exists(
        &self,
        session: Option<&diri_proto::SessionId>,
    ) -> Result<(), ControlError> {
        if let Some(id) = session {
            let registry = self.registry.lock().map_err(poisoned)?;
            if registry.record(&id.0).is_none() {
                return Err(ControlError::not_found(format!("no session {}", id.0)));
            }
        }
        Ok(())
    }

    pub(super) fn sandbox_call(
        &self,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, ControlError> {
        match method {
            Method::SANDBOX_START => {
                let p: SandboxStartParams = decode(params)?;
                self.sandbox_caller_exists(p.session_id.as_ref())?;
                let info = self
                    .sandbox_pool()
                    .start(Caller::from_option(p.session_id.as_ref()), &p)
                    .map_err(control_error)?;
                self.publish_sandboxes_changed();
                encode(&info)
            }
            Method::SANDBOX_ACT => {
                let p: SandboxActParams = decode(params)?;
                let pool = self.existing_pool()?;
                pool.act(
                    Caller::from_option(p.session_id.as_ref()),
                    &p.sandbox_id,
                    &p.action,
                )
                .map_err(control_error)
            }
            Method::SANDBOX_SNAPSHOT => {
                let p: SandboxSnapshotParams = decode(params)?;
                let pool = self.existing_pool()?;
                encode(
                    &pool
                        .snapshot(
                            Caller::from_option(p.session_id.as_ref()),
                            &p.sandbox_id,
                            p.label.as_deref(),
                            p.inline,
                        )
                        .map_err(control_error)?,
                )
            }
            Method::SANDBOX_STOP => {
                let p: SandboxStopParams = decode(params)?;
                let pool = self.existing_pool()?;
                pool.stop(Caller::from_option(p.session_id.as_ref()), &p.sandbox_id)
                    .map_err(control_error)?;
                self.publish_sandboxes_changed();
                Ok(serde_json::json!({}))
            }
            Method::SANDBOX_LIST => {
                let _: SandboxListParams = decode(params.or(Some(serde_json::json!({}))))?;
                // Listing never starts the pool; backend discovery is cheap.
                match self.sandbox.get() {
                    Some(pool) => encode(&pool.list()),
                    None => encode(&self.sandbox_pool().list()),
                }
            }
            Method::SANDBOX_FRAME => {
                let p: SandboxFrameParams = decode(params)?;
                encode(
                    &self
                        .existing_pool()?
                        .frame(&p.sandbox_id, p.after_seq)
                        .map_err(control_error)?,
                )
            }
            other => Err(ControlError::bad_request(format!("unknown method {other}"))),
        }
    }

    fn existing_pool(&self) -> Result<&SandboxPool, ControlError> {
        self.sandbox.get().ok_or_else(|| {
            ControlError::new(
                diri_proto::sandbox::NOT_FOUND,
                "no sandbox is running; start one with sandbox.start",
            )
        })
    }

    /// The app refreshes its Ghost Desktops view on this.
    fn publish_sandboxes_changed(&self) {
        let occupied = self.sandbox.get().map_or(0, SandboxPool::occupied);
        self.events.publish(
            diri_proto::EventName::SANDBOX_CHANGED,
            serde_json::json!({ "occupied": occupied }),
            None,
        );
    }

    /// `session.remove`: a closed session's sandboxes go with it, now
    /// rather than at the reaper's next beat.
    pub(super) fn release_session_sandboxes(&self, session: &str) {
        if let Some(pool) = self.sandbox.get()
            && pool.release_owner(session) > 0
        {
            self.publish_sandboxes_changed();
        }
    }
}
