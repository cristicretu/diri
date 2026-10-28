//! Settings > General > Import: bring a terminal setup over from another
//! terminal, and sessions over from another agent manager.
//!
//! The terminal row reads the store's latest scan of other terminals'
//! configs; Review… opens the same one-screen sheet the first-run welcome
//! offers, and Undo takes back the import applied this run. The herdr row
//! only appears once a scan has found herdr on this machine.
use super::*;

impl UtilitySurfaces {
    pub(super) fn import_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = self.settings_colors();
        let terminal = self.terminal_setup_row(colors, cx);
        let herdr = self.herdr_import_row(colors, cx);
        let mut rows = div().flex().flex_col().child(terminal);
        if let Some(herdr) = herdr {
            rows = rows.child(setting_divider(colors)).child(herdr);
        }
        Some(setting_section(t("settings.import.title"), rows, colors).into_any_element())
    }

    /// Settings keeps a copy of the preferences; something else (the import
    /// sheet) changed them.
    pub(crate) fn reload_preferences(&mut self, cx: &mut Context<Self>) {
        self.prefs = self
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .clone();
        cx.notify();
    }

    fn terminal_setup_row(&self, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let (scanning, scanned, applied, plan, found) = {
            let store = self.store.read().expect("session store lock poisoned");
            let setup = store.terminal_setup();
            (
                setup.scanning,
                setup.scanned(),
                setup.applied.as_ref().map(|undo| undo.source),
                setup.plans().first().cloned(),
                setup.found(),
            )
        };
        let detail = match (&plan, applied, found) {
            (_, Some(source), _) => tf(
                "terminal_import.settings.imported",
                &[("terminal", &source.name())],
            ),
            _ if !scanned || scanning && plan.is_none() => {
                t("terminal_import.settings.looking").to_owned()
            }
            (Some(plan), _, _) => tf(
                "terminal_import.settings.summary",
                &[("terminal", &plan.source.name()), ("changes", &plan.summary())],
            ),
            (None, _, Some(source)) => tf(
                "terminal_import.settings.matches",
                &[("terminal", &source.name())],
            ),
            (None, _, None) => tf(
                "terminal_import.settings.none_found",
                &[("machine", &crate::platform::local_machine_label_lowercase())],
            ),
        };
        let control = if applied.is_some() {
            surface_button(
                t("terminal_import.settings.undo"),
                "terminal-import-undo",
                colors,
                cx,
                |_, cx| cx.emit(UtilitySurfacesEvent::UndoTerminalImport),
            )
            .into_any_element()
        } else if plan.is_some() {
            surface_button(
                t("terminal_import.settings.review"),
                "terminal-import-review",
                colors,
                cx,
                |_, cx| cx.emit(UtilitySurfacesEvent::ShowTerminalImport),
            )
            .into_any_element()
        } else {
            div().into_any_element()
        };
        setting_row(t("terminal_import.settings.row"), detail, control, colors).into_any_element()
    }

    fn herdr_import_row(&self, colors: SemanticColors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (plan, scanning, importing) = {
            let store = self.store.read().expect("session store lock poisoned");
            let herdr = store.herdr();
            (herdr.plan.clone(), herdr.scanning, herdr.importing)
        };
        if !importing && !plan.as_ref().is_some_and(|plan| plan.found) {
            return None;
        }
        let detail = match &plan {
            _ if importing => t("settings.import.opening").to_owned(),
            None => t("settings.import.looking").to_owned(),
            Some(plan) if !plan.found => tf(
                "settings.import.none_found",
                &[("machine", &crate::platform::local_machine_label_lowercase())],
            ),
            Some(plan) if plan.is_empty() => t("settings.import.all_imported").to_owned(),
            Some(plan) => plan.summary(),
        };
        let control = match plan.filter(|plan| !plan.is_empty()) {
            Some(plan) if !importing => surface_button_with_window(
                t("settings.import.import"),
                "import-herdr",
                colors,
                cx,
                move |this, window, cx| {
                    let store = this.store.clone();
                    crate::herdr_import::confirm(&plan, window, cx, move |cx| {
                        store
                            .write()
                            .expect("session store lock poisoned")
                            .import_herdr();
                        cx.refresh_windows();
                    });
                },
            )
            .into_any_element(),
            _ if importing || scanning => div().into_any_element(),
            _ => surface_button(
                t("settings.import.check_again"),
                "import-herdr-rescan",
                colors,
                cx,
                |this, cx| {
                    this.store
                        .write()
                        .expect("session store lock poisoned")
                        .request_herdr_scan();
                    cx.notify();
                },
            )
            .into_any_element(),
        };
        Some(setting_row("herdr", detail, control, colors).into_any_element())
    }
}
