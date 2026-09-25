//! Store side of the herdr import: the latest plan, and running it.
//!
//! Planning reads a few small files plus a transcript lookup per
//! conversation, so it runs on a blocking thread and lands back here. The
//! import itself is a sequence of ordinary Engine calls: a resume for every
//! conversation, a spawn for everything else, one at a time so sessions land
//! in the sidebar in herdr's order.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use diri_client::DaemonClient;
use diri_proto::{AgentKind, HistoryEntry, SessionId, SessionSpawnParams};
use tokio::sync::broadcast;

use super::{SessionStore, SpawnOptions, StoreEffect};
use crate::herdr_import::{self, HerdrAction, HerdrPlan, HerdrRoots};
use crate::notifications::StatusTransition;

#[derive(Debug, Default)]
pub struct HerdrState {
    /// `None` until the first scan answers.
    pub plan: Option<HerdrPlan>,
    pub scanning: bool,
    pub importing: bool,
}

/// One pane's Engine call, resolved against this store's spawn defaults when
/// the user confirmed, so a slow import cannot pick up a changed default.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportStep {
    pub remember: Vec<String>,
    pub call: ImportCall,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportCall {
    Resume(HistoryEntry),
    Spawn(SessionSpawnParams),
}

impl SessionStore {
    pub fn herdr(&self) -> &HerdrState {
        &self.herdr
    }

    #[cfg(test)]
    pub(crate) fn set_herdr_plan(&mut self, plan: Option<HerdrPlan>) {
        self.herdr.plan = plan;
        self.herdr.scanning = false;
    }

    /// Look for herdr sessions again. Cheap enough to run whenever a surface
    /// that offers the import appears.
    pub fn request_herdr_scan(&mut self) {
        if self.herdr.scanning || self.herdr.importing {
            return;
        }
        self.herdr.scanning = true;
        let tracked = self
            .sessions
            .values()
            .filter_map(|session| session.agent_session_id.clone())
            .collect();
        let imported = self.prefs.herdr_imported.iter().cloned().collect();
        self.emit(StoreEffect::ScanHerdr { tracked, imported });
    }

    /// Start importing the current plan. Returns false when there is nothing
    /// to import or an import is already running.
    pub fn import_herdr(&mut self) -> bool {
        if self.herdr.importing {
            return false;
        }
        let Some(plan) = self.herdr.plan.as_ref().filter(|plan| !plan.is_empty()) else {
            return false;
        };
        let steps = plan
            .items
            .iter()
            .map(|item| {
                let spawn = |kind: AgentKind| {
                    self.spawn_params(
                        kind,
                        SpawnOptions {
                            cwd: Some(item.cwd.clone()),
                            title: item.title.clone(),
                            ..SpawnOptions::default()
                        },
                    )
                };
                let call = match &item.action {
                    HerdrAction::Resume(entry) => {
                        let mut entry = entry.clone();
                        // herdr's own name for the pane beats a first prompt.
                        if item.title.is_some() {
                            entry.title.clone_from(&item.title);
                        }
                        ImportCall::Resume(entry)
                    }
                    HerdrAction::Start(kind) => ImportCall::Spawn(spawn(kind.clone())),
                    HerdrAction::Terminal => ImportCall::Spawn(spawn(AgentKind::SHELL)),
                };
                ImportStep {
                    remember: herdr_import::remembered_keys(item),
                    call,
                }
            })
            .collect();
        self.herdr.importing = true;
        self.emit(StoreEffect::ImportHerdr(steps));
        true
    }
}

pub(super) async fn scan(
    tracked: HashSet<String>,
    imported: HashSet<String>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
) {
    let plan = tokio::task::spawn_blocking(move || {
        herdr_import::plan(&HerdrRoots::current_user(), &tracked, &imported)
    })
    .await
    .unwrap_or_default();
    let mut locked = store.write().expect("session store lock poisoned");
    locked.herdr.scanning = false;
    locked.herdr.plan = Some(plan);
    drop(locked);
    let _ = change_tx.send(());
}

pub(super) async fn import(
    steps: Vec<ImportStep>,
    client: Arc<DaemonClient>,
    store: Arc<RwLock<SessionStore>>,
    change_tx: broadcast::Sender<()>,
    status_tx: broadcast::Sender<StatusTransition>,
) {
    let total = steps.len();
    let mut opened: Vec<SessionId> = Vec::new();
    let mut remembered = Vec::new();
    let mut failures = Vec::new();
    for step in steps {
        let result = match step.call {
            ImportCall::Resume(entry) => client
                .resume_from_history(entry)
                .await
                .map(|record| record.id),
            ImportCall::Spawn(params) => client.spawn(params).await,
        };
        match result {
            Ok(id) => {
                opened.push(id);
                remembered.extend(step.remember);
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
    let mut locked = store.write().expect("session store lock poisoned");
    locked.herdr.importing = false;
    // Best effort: a prefs write that fails only means a later import offers
    // these panes again, which the sidebar makes obvious.
    let _ = locked.update_preferences(|prefs| prefs.herdr_imported.extend(remembered));
    if let Some(first) = opened.first() {
        locked.apply_spawn_result(first.clone());
    }
    locked.herdr.plan = None;
    locked.request_herdr_scan();
    drop(locked);
    let _ = change_tx.send(());
    let _ = status_tx.send(crate::notifications::herdr_import_transition(
        opened.len(),
        total,
        failures.first().map(String::as_str),
    ));
}
