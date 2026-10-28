//! Store side of importing another terminal's setup: the latest scan, the
//! plans made from it against the current preferences, and the one applied
//! import that Undo can still take back.
//!
//! Scanning reads a handful of small files, on a blocking thread. It runs
//! only when a surface that offers the import is showing (the first-run
//! welcome, Settings › General), never per session or on a timer.

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use super::SessionStore;
use crate::terminal_import::{self, Detected, Environment, Plan, Roots, Source, Undo};

#[derive(Debug, Default)]
pub(crate) struct TerminalSetupState {
    /// `None` until the first scan answers.
    detected: Option<Vec<Detected>>,
    pub(crate) scanning: bool,
    /// Plans against the current preferences, best first.
    plans: Vec<Plan>,
    /// The import applied this run, until it is undone.
    pub(crate) applied: Option<Undo>,
}

impl TerminalSetupState {
    pub(crate) fn scanned(&self) -> bool {
        self.detected.is_some()
    }

    pub(crate) fn plans(&self) -> &[Plan] {
        &self.plans
    }

    /// Terminals found at all, changes or not, for "Already matches …".
    pub(crate) fn found(&self) -> Option<Source> {
        self.detected
            .as_ref()
            .and_then(|found| found.iter().find(|found| found.source != Source::TerminalApp))
            .or_else(|| self.detected.as_ref().and_then(|found| found.first()))
            .map(|found| found.source)
    }
}

impl SessionStore {
    pub(crate) fn terminal_setup(&self) -> &TerminalSetupState {
        &self.terminal_setup
    }

    /// Look for other terminals' configs again.
    pub(crate) fn request_terminal_setup_scan(&mut self) {
        if self.terminal_setup.scanning {
            return;
        }
        self.terminal_setup.scanning = true;
        self.emit(super::StoreEffect::ScanTerminalSetup);
    }

    pub(crate) fn set_terminal_setup_scan(&mut self, detected: Vec<Detected>) {
        self.terminal_setup.scanning = false;
        self.terminal_setup.detected = Some(detected);
        self.replan_terminal_setup();
    }

    /// Plans follow the preferences: what "will change" is always against
    /// what Diri has now.
    pub(crate) fn replan_terminal_setup(&mut self) {
        let Some(detected) = &self.terminal_setup.detected else {
            return;
        };
        let resolve = |name: &str| crate::fonts::installed_family(name);
        let env = Environment {
            resolve_font: &resolve,
        };
        self.terminal_setup.plans = terminal_import::plans(detected, &self.prefs, &env);
    }

    /// Applies the plan for `source` as one preference write. Returns the
    /// source's name on success for the confirmation.
    pub(crate) fn apply_terminal_setup(&mut self, source: Source) -> Result<(), String> {
        self.replan_terminal_setup();
        let Some(plan) = self
            .terminal_setup
            .plans
            .iter()
            .find(|plan| plan.source == source)
            .cloned()
        else {
            return Err("nothing to import".to_owned());
        };
        let current = self.prefs.clone();
        let dir = crate::imported_themes::default_dir();
        let result = terminal_import::apply(&plan, &dir, &current, |edit| {
            self.update_preferences(|prefs| edit(prefs))
        });
        match result {
            Ok(undo) => {
                diri_telemetry::event!(
                    "terminal_import.applied",
                    source = source.name(),
                    changes = plan.changes.len() as u64
                );
                self.terminal_setup.applied = Some(undo);
                self.replan_terminal_setup();
                Ok(())
            }
            Err(error) => {
                diri_telemetry::warn_event!(
                    "terminal_import.failed",
                    source = source.name(),
                    reason = match error {
                        terminal_import::ApplyError::Theme => "theme",
                        terminal_import::ApplyError::Prefs => "prefs",
                    }
                );
                Err(format!("{error:?}"))
            }
        }
    }

    /// Puts back what the last import replaced. False when there is none.
    pub(crate) fn undo_terminal_setup(&mut self) -> bool {
        let Some(undo) = self.terminal_setup.applied.take() else {
            return false;
        };
        let result =
            terminal_import::undo(&undo, |edit| self.update_preferences(|prefs| edit(prefs)));
        if result.is_err() {
            self.terminal_setup.applied = Some(undo);
            return false;
        }
        self.replan_terminal_setup();
        true
    }
}

pub(super) async fn scan(store: Arc<RwLock<SessionStore>>, change_tx: broadcast::Sender<()>) {
    let detected = tokio::task::spawn_blocking(|| terminal_import::scan(&Roots::current_user()))
        .await
        .unwrap_or_default();
    store
        .write()
        .expect("session store lock poisoned")
        .set_terminal_setup_scan(detected);
    let _ = change_tx.send(());
}
