//! Settings > General > Import: bring sessions over from another agent
//! manager. The row reads the store's latest herdr scan; the import itself is
//! the same confirmed store action the first-run welcome offers.
use super::*;

impl UtilitySurfaces {
    pub(super) fn import_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.settings_colors();
        let (plan, scanning, importing) = {
            let store = self.store.read().expect("session store lock poisoned");
            let herdr = store.herdr();
            (herdr.plan.clone(), herdr.scanning, herdr.importing)
        };
        let detail = match &plan {
            _ if importing => "Opening sessions…".to_owned(),
            None => "Looking for herdr sessions…".to_owned(),
            Some(plan) if !plan.found => "No herdr sessions on this Mac.".to_owned(),
            Some(plan) if plan.is_empty() => "Everything from herdr is already here.".to_owned(),
            Some(plan) => plan.summary(),
        };
        let control = match plan.filter(|plan| !plan.is_empty()) {
            Some(plan) if !importing => surface_button_with_window(
                "Import…",
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
                "Check Again",
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
        setting_section(
            "Import",
            setting_row("herdr", detail, control, colors),
            colors,
        )
    }
}
