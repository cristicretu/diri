//! One switch for every local account: pick a profile, every open tab of that
//! Agent follows it. Codex swaps the shared login file; Claude points its
//! credential store at the profile's slot. Both relaunch through here.
use super::*;
use diri_proto::{AgentAccountProfile, AgentKind, SwitchAccountResult};

/// An open tab whose relaunch is fully planned before anything is stopped.
pub(super) struct PreparedTab {
    pub(super) record: diri_proto::SessionRecord,
    pub(super) running: bool,
    pub(super) spec: crate::session::SessionSpec,
}

impl ControlServer {
    pub(super) fn account_switch_all(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: diri_proto::SwitchAccountParams = decode(params)?;
        let profile = self
            .accounts
            .lock()
            .map_err(poisoned)?
            .catalog()?
            .profiles
            .into_iter()
            .find(|profile| profile.id == p.account_profile_id)
            .ok_or_else(|| {
                ControlError::not_found("The selected account profile no longer exists")
            })?;
        if profile.host.is_some() {
            return Err(ControlError::bad_request(
                "Remote profiles are chosen per conversation; the shared switch applies to this Mac.",
            ));
        }
        match profile.agent.as_str() {
            AgentKind::CODEX_ID => self.switch_codex(profile),
            AgentKind::CLAUDE_CODE_ID => self.switch_claude(profile),
            _ => Err(ControlError::bad_request(
                "Account switching supports Claude Code and Codex",
            )),
        }
    }

    /// Local, unarchived tabs of `kind` that are open in a Diri workspace.
    pub(super) fn open_local_tabs(
        &self,
        kind: &AgentKind,
    ) -> Result<Vec<diri_proto::SessionRecord>, ControlError> {
        let open = self.workspaces.snapshot()?.open_session_ids();
        let records = self.registry.lock().map_err(poisoned)?.records();
        Ok(records
            .into_iter()
            .filter(|r| {
                &r.kind == kind && r.host.is_none() && !r.is_archived() && open.contains(&r.id)
            })
            .collect())
    }

    /// Relaunch every prepared tab on `profile` once the login is installed.
    /// Sleeping tabs come back asleep, stopped tabs stay stopped, and a tab
    /// whose relaunch fails is reported rather than blocking the rest.
    pub(super) fn relaunch_switched(
        &self,
        prepared: Vec<PreparedTab>,
        profile: &AgentAccountProfile,
        installed: bool,
        result: &mut SwitchAccountResult,
    ) {
        for PreparedTab {
            record,
            running,
            spec,
        } in prepared
        {
            let change = (|| {
                let mut registry = self.registry.lock().map_err(poisoned)?;
                if installed {
                    registry.update_record(&record.id.0, |r| {
                        r.account_profile = Some(profile.clone());
                        r.hibernation = None;
                    });
                }
                registry.persist_now().map_err(io_control_error)?;
                if running
                    && (registry.get(&record.id.0).is_none()
                        || registry.record(&record.id.0).is_some_and(|r| {
                            matches!(r.status, diri_proto::SessionStatus::Exited(_))
                        }))
                {
                    registry.respawn(spec).map_err(io_control_error)?;
                    if let Some(sleep) = record.hibernation {
                        registry
                            .hibernate(&record.id.0, sleep.reason)
                            .map_err(io_control_error)?;
                    }
                }
                self.publish_updated(&registry, &record.id.0);
                registry.persist_now().map_err(io_control_error)?;
                registry
                    .record(&record.id.0)
                    .ok_or_else(|| ControlError::internal("Switched session vanished"))
            })();
            match change {
                Ok(r) if installed => result.switched.push(r),
                Ok(r) => result.unchanged.push(r.id),
                Err(e) => result.failures.push(diri_proto::AccountSwitchFailure {
                    session_id: record.id,
                    message: e.message,
                }),
            }
        }
    }
}
