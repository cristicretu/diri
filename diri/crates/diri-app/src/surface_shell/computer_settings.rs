//! Settings › Agents › Computer use: one place to set computer use up and
//! allow it, for people who have never opened a terminal.
//!
//! Set up downloads the driver in the background, then walks through the two
//! macOS grants with a guide anchored to System Settings: drag Diri into the
//! list (the driver runs as part of Diri, so the grants are Diri's), and the
//! next step follows on its own. Screen Recording applies to a process only
//! from its launch, so while that step waits the driver is restarted every
//! few seconds until it reports the grant.
use super::*;
use std::time::Instant;

/// How often an open guide re-checks the grants.
const GUIDE_TICK: Duration = Duration::from_millis(400);
/// How long each tick keeps the Engine polling.
const GUIDE_WATCH: Duration = Duration::from_secs(3);
/// While Screen Recording waits: how often the driver is relaunched so a
/// grant made meanwhile takes effect.
const SCREEN_RESTART_EVERY: Duration = Duration::from_secs(4);
/// How long the section keeps polling after it was last painted.
const SECTION_WATCH: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GrantStep {
    Accessibility,
    ScreenRecording,
}

#[derive(Default)]
pub(super) struct ComputerGuide {
    step: Option<GrantStep>,
    ticker: Option<Task<()>>,
    last_restart: Option<Instant>,
    /// The log is shown under a failed install.
    log_open: bool,
}

impl GrantStep {
    fn granted(self, status: &diri_proto::ComputerStatus) -> bool {
        match self {
            Self::Accessibility => status.accessibility == Some(true),
            Self::ScreenRecording => status.screen_recording == Some(true),
        }
    }

    /// The first grant still missing.
    fn next(status: &diri_proto::ComputerStatus) -> Option<Self> {
        [Self::Accessibility, Self::ScreenRecording]
            .into_iter()
            .find(|step| !step.granted(status))
    }

    fn title(self, app: &str) -> String {
        let key = match self {
            Self::Accessibility => "settings.computer.guide_accessibility",
            Self::ScreenRecording => "settings.computer.guide_screen_recording",
        };
        tf(key, &[("app", &app)])
    }
}

impl UtilitySurfaces {
    /// Called every render: opens the first guide after a Set up install.
    pub(super) fn sync_computer_setup(&mut self, cx: &mut Context<Self>) {
        let start = {
            let mut store = self.store.write().expect("session store lock poisoned");
            if self.surface == Surface::Settings && self.settings_tab == SettingsTab::Agents {
                store.watch_computer(SECTION_WATCH);
            }
            store
                .take_guide_after_install()
                .then(|| store.computer_status().and_then(GrantStep::next))
                .flatten()
        };
        if let Some(step) = start {
            self.open_grant_guide(step, cx);
        }
    }

    fn open_grant_guide(&mut self, step: GrantStep, cx: &mut Context<Self>) {
        let app = self
            .store
            .read()
            .expect("session store lock poisoned")
            .computer_status()
            .and_then(|status| status.app_path.clone())
            .map(std::path::PathBuf::from)
            // A loose Engine build: the driver still runs under this app.
            .or_else(own_bundle);
        let Some(app) = app else {
            return;
        };
        if !present_guide(step, &app) {
            return;
        }
        self.computer_guide.step = Some(step);
        self.computer_guide.last_restart = None;
        if self.computer_guide.ticker.is_none() {
            self.computer_guide.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(GUIDE_TICK).await;
                    let keep = this
                        .update(cx, |this, cx| this.tick_grant_guide(cx))
                        .unwrap_or(false);
                    if !keep {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// One look at the grants while a guide is open. Returns whether to
    /// keep looking.
    fn tick_grant_guide(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(step) = self.computer_guide.step else {
            self.computer_guide.ticker = None;
            return false;
        };
        if !guide_presented() {
            // Closed with its ✕.
            self.computer_guide.step = None;
            self.computer_guide.ticker = None;
            cx.notify();
            return false;
        }
        let status = {
            let mut store = self.store.write().expect("session store lock poisoned");
            store.watch_computer(GUIDE_WATCH);
            store.computer_status().cloned()
        };
        let Some(status) = status else {
            return true;
        };
        if step.granted(&status) {
            match GrantStep::next(&status) {
                Some(next) => {
                    self.open_grant_guide(next, cx);
                    return true;
                }
                None => {
                    dismiss_guide();
                    self.computer_guide.step = None;
                    self.computer_guide.ticker = None;
                    // Back to Diri, where the section now says Ready.
                    cx.activate(true);
                    cx.notify();
                    return false;
                }
            }
        }
        if step == GrantStep::ScreenRecording
            && status.accessibility == Some(true)
            && self
                .computer_guide
                .last_restart
                .is_none_or(|at| at.elapsed() >= SCREEN_RESTART_EVERY)
        {
            self.computer_guide.last_restart = Some(Instant::now());
            self.store
                .write()
                .expect("session store lock poisoned")
                .computer_setup(diri_proto::ComputerSetupAction::Restart);
        }
        true
    }

    fn close_grant_guide(&mut self, cx: &mut Context<Self>) {
        dismiss_guide();
        self.computer_guide.step = None;
        self.computer_guide.ticker = None;
        cx.notify();
    }

    /// Screenshot fixtures: one named setup state, no Engine behind it.
    #[cfg(test)]
    pub(super) fn seed_computer_preview(&mut self, state: &str) {
        let mut status = diri_proto::ComputerStatus {
            supported: true,
            version: Some("0.36.0".into()),
            app_path: Some("/Applications/Diri.app".into()),
            tested_version: "0.36.0".into(),
            accessibility: Some(false),
            screen_recording: Some(false),
            ..Default::default()
        };
        match state {
            "missing" => {
                status.version = None;
                status.accessibility = None;
                status.screen_recording = None;
            }
            "installing" => {
                status.version = None;
                status.installing = true;
            }
            "failed" => {
                status.version = None;
                status.install_error = Some(
                    "Couldn't reach the download. Check the connection and try again.".into(),
                );
                status.install_log = Some(
                    "==> downloading https://github.com/trycua/cua/releases/download/\
                     cua-driver-rs-v0.36.0/cua-driver-rs-0.36.0-darwin-universal-binary.tar.gz\n\
                     curl: (6) Could not resolve host: github.com\n"
                        .into(),
                );
                self.computer_guide.log_open = true;
            }
            "waiting" => {
                status.accessibility = Some(true);
                self.computer_guide.step = Some(GrantStep::ScreenRecording);
            }
            "ready" => {
                status.accessibility = Some(true);
                status.screen_recording = Some(true);
            }
            _ => {}
        }
        self.store
            .write()
            .expect("session store lock poisoned")
            .seed_computer_status(status);
    }

    /// The section itself. Only this Mac, and only where it exists.
    pub(super) fn computer_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.agents_host.is_some() {
            return None;
        }
        let colors = self.settings_colors();
        let (status, error, busy) = {
            let store = self.store.read().expect("session store lock poisoned");
            (
                store.computer_status().cloned(),
                store.computer_status_error().map(str::to_owned),
                store.computer_busy(),
            )
        };
        if status.as_ref().is_some_and(|status| !status.supported) {
            return None;
        }
        let mut body = div().flex().flex_col();
        let headline = match &status {
            None if error.is_some() => setting_row(
                t("settings.computer.title"),
                t("settings.computer.engine_unavailable"),
                div(),
                colors,
            )
            .into_any_element(),
            None => setting_row(
                t("settings.computer.title"),
                t("settings.computer.detail"),
                status_label(t("settings.computer.checking"), colors.tertiary),
                colors,
            )
            .into_any_element(),
            Some(status) if status.installing => setting_row(
                t("settings.computer.title"),
                t("settings.computer.installing_detail"),
                status_label(t("settings.computer.installing"), colors.secondary),
                colors,
            )
            .into_any_element(),
            Some(status) if !status.installed() || status.outdated => {
                let update = status.installed();
                setting_row(
                    t("settings.computer.title"),
                    match &status.install_error {
                        Some(error) => error.clone(),
                        None if update => tf(
                            "settings.computer.update_detail",
                            &[
                                ("installed", &status.version.clone().unwrap_or_default()),
                                ("tested", &status.tested_version),
                            ],
                        ),
                        None => t("settings.computer.detail").to_owned(),
                    },
                    surface_button(
                        if busy {
                            t("settings.computer.installing")
                        } else if status.install_error.is_some() {
                            t("settings.computer.try_again")
                        } else if update {
                            t("settings.computer.update")
                        } else {
                            t("settings.computer.set_up")
                        },
                        "computer-set-up",
                        colors,
                        cx,
                        |this, cx| {
                            this.computer_guide.log_open = false;
                            this.store
                                .write()
                                .expect("session store lock poisoned")
                                .computer_setup(diri_proto::ComputerSetupAction::Install);
                            cx.notify();
                        },
                    ),
                    colors,
                )
                .into_any_element()
            }
            Some(status) => setting_row(
                t("settings.computer.title"),
                if status.ready() {
                    tf(
                        "settings.computer.ready_detail",
                        &[("version", &status.version.clone().unwrap_or_default())],
                    )
                } else {
                    t("settings.computer.allow_detail").to_owned()
                },
                if status.ready() {
                    status_label(t("settings.computer.ready"), Ink::FRESH).into_any_element()
                } else {
                    div().into_any_element()
                },
                colors,
            )
            .into_any_element(),
        };
        body = body.child(headline);

        if let Some(status) = status.as_ref().filter(|status| {
            status.installed() && !status.installing && !status.outdated
        }) {
            for step in [GrantStep::Accessibility, GrantStep::ScreenRecording] {
                body = body
                    .child(setting_divider(colors))
                    .child(self.grant_row(step, status, colors, cx));
            }
        }
        if let Some(log) = status
            .as_ref()
            .filter(|status| status.install_error.is_some())
            .and_then(|status| status.install_log.clone())
        {
            body = body.child(setting_divider(colors)).child(setting_row(
                t("settings.computer.log"),
                t("settings.computer.log_detail"),
                surface_button(
                    if self.computer_guide.log_open {
                        t("settings.computer.hide_log")
                    } else {
                        t("settings.computer.show_log")
                    },
                    "computer-install-log",
                    colors,
                    cx,
                    |this, cx| {
                        this.computer_guide.log_open = !this.computer_guide.log_open;
                        cx.notify();
                    },
                ),
                colors,
            ));
            if self.computer_guide.log_open {
                body = body.child(
                    div()
                        .id("computer-install-log-text")
                        .max_h(px(220.0))
                        .overflow_y_scroll()
                        .px(px(12.0))
                        .py(px(8.0))
                        .font_family(crate::fonts::mono_family())
                        .text_size(px(10.0))
                        .text_color(colors.secondary)
                        .whitespace_normal()
                        .child(log),
                );
            }
        }
        Some(setting_section(t("settings.computer.section"), body, colors).into_any_element())
    }

    fn grant_row(
        &self,
        step: GrantStep,
        status: &diri_proto::ComputerStatus,
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, detail, id) = match step {
            GrantStep::Accessibility => (
                t("settings.computer.accessibility"),
                t("settings.computer.accessibility_detail"),
                "computer-allow-accessibility",
            ),
            GrantStep::ScreenRecording => (
                t("settings.computer.screen_recording"),
                t("settings.computer.screen_recording_detail"),
                "computer-allow-screen-recording",
            ),
        };
        let control = if step.granted(status) {
            status_label(t("settings.computer.allowed"), Ink::FRESH).into_any_element()
        } else if self.computer_guide.step == Some(step) {
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(status_label(
                    t("settings.computer.waiting"),
                    colors.secondary,
                ))
                .child(surface_button(
                    t("settings.computer.cancel"),
                    format!("{id}-cancel"),
                    colors,
                    cx,
                    |this, cx| this.close_grant_guide(cx),
                ))
                .into_any_element()
        } else {
            surface_button(t("settings.computer.allow"), id, colors, cx, move |this, cx| {
                this.open_grant_guide(step, cx);
            })
            .into_any_element()
        };
        setting_row(label, detail, control, colors).into_any_element()
    }
}

fn status_label(text: &'static str, color: impl Into<gpui::Hsla>) -> impl IntoElement {
    div()
        .flex_none()
        .text_size(px(11.0))
        .text_color(color.into())
        .child(text)
}

/// The `.app` this process runs from.
fn own_bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .skip(1)
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .map(std::path::Path::to_path_buf)
}

/// "Diri", or "Diri Dev": the name System Settings lists the bundle under.
fn bundle_name(app: &std::path::Path) -> String {
    app.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Diri".to_owned())
}

#[cfg(target_os = "macos")]
fn present_guide(step: GrantStep, app: &std::path::Path) -> bool {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return false;
    };
    let pane = match step {
        GrantStep::Accessibility => crate::macos::permission_guide::Pane::Accessibility,
        GrantStep::ScreenRecording => crate::macos::permission_guide::Pane::ScreenRecording,
    };
    let name = bundle_name(app);
    crate::macos::permission_guide::present(
        mtm,
        pane,
        app,
        &name,
        &step.title(&name),
        // The guide closes itself; the next tick notices.
        Box::new(|| {}),
    );
    true
}

#[cfg(not(target_os = "macos"))]
fn present_guide(_step: GrantStep, _app: &std::path::Path) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn guide_presented() -> bool {
    crate::macos::permission_guide::is_presented()
}

#[cfg(not(target_os = "macos"))]
fn guide_presented() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn dismiss_guide() {
    crate::macos::permission_guide::dismiss();
}

#[cfg(not(target_os = "macos"))]
fn dismiss_guide() {}
