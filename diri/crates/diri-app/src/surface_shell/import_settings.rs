//! Settings > General > Import: bring sessions over from another agent
//! manager. The row reads the store's latest herdr scan; the import itself is
//! the same confirmed store action the first-run welcome offers. It only
//! appears once the scan has found herdr on this machine.
use super::*;

impl UtilitySurfaces {
    pub(super) fn import_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = self.settings_colors();
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
            Some(plan) if !plan.found => {
                tf("settings.import.none_found", &[("machine", &crate::platform::local_machine_label_lowercase())])
            }
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
        Some(
            setting_section(
                t("settings.import.title"),
                setting_row("herdr", detail, control, colors),
                colors,
            )
            .into_any_element(),
        )
    }
}
