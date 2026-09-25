//! "Start diri at login", registered with macOS rather than merely remembered.
//!
//! The operating system owns the truth: the user can approve, deny, or remove
//! the login item in System Settings at any time. The saved preference is only
//! a mirror of that, so every path here ends by reading the registration back
//! and reporting what macOS says, never what was asked for.

/// What macOS reports for the main app's login item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoginItemStatus {
    Enabled,
    NotRegistered,
    /// Registered, but held until the user allows it under System Settings >
    /// General > Login Items. It will not launch at login yet.
    RequiresApproval,
    /// There is nothing to register: this is not the bundled app (a `cargo run`
    /// binary, a test, a development bundle), the OS predates `SMAppService`,
    /// or it is not macOS.
    Unavailable,
}

/// A refused registration. Only the numeric code crosses the seam; the
/// system's own description can carry paths, so it is never shown or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoginItemError {
    pub(crate) code: isize,
}

/// `kSMErrorInvalidSignature`
const ERROR_INVALID_SIGNATURE: isize = 3;
/// `kSMErrorLaunchDeniedByUser`
const ERROR_LAUNCH_DENIED_BY_USER: isize = 11;

/// The operating system calls, behind a seam so the reconciliation can be
/// tested without ServiceManagement or a signed bundle.
pub(crate) trait LoginItemBackend {
    fn status(&self) -> LoginItemStatus;
    fn register(&self) -> Result<(), LoginItemError>;
    fn unregister(&self) -> Result<(), LoginItemError>;
    /// Opens System Settings > General > Login Items.
    fn open_approval_settings(&self);
}

/// What Settings shows for the login item: the registration as macOS last
/// reported it, and the refusal from the change that was just attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoginItemState {
    pub(crate) status: LoginItemStatus,
    pub(crate) failure: Option<LoginItemError>,
}

impl LoginItemState {
    /// Whether the toggle is on. Only a registration macOS will actually act
    /// on counts; one still waiting for approval does not.
    pub(crate) fn enabled(self) -> bool {
        self.status == LoginItemStatus::Enabled
    }

    pub(crate) fn available(self) -> bool {
        self.status != LoginItemStatus::Unavailable
    }

    /// What `start_at_login` should hold given the saved value. An unavailable
    /// backend knows nothing about the installed app's registration, so a
    /// development build leaves the preference alone.
    pub(crate) fn preference(self, saved: bool) -> bool {
        if self.available() {
            self.enabled()
        } else {
            saved
        }
    }

    pub(crate) fn failed(self) -> bool {
        self.failure.is_some()
    }

    /// The row's second line.
    pub(crate) fn detail(self) -> String {
        if let Some(error) = self.failure {
            return match error.code {
                ERROR_INVALID_SIGNATURE => {
                    "macOS only starts a signed copy of diri at login.".to_owned()
                }
                ERROR_LAUNCH_DENIED_BY_USER => APPROVAL_DETAIL.to_owned(),
                code => format!("macOS could not update Login Items (error {code})."),
            };
        }
        match self.status {
            LoginItemStatus::Enabled | LoginItemStatus::NotRegistered => {
                "Open diri automatically after you sign in.".to_owned()
            }
            LoginItemStatus::RequiresApproval => APPROVAL_DETAIL.to_owned(),
            LoginItemStatus::Unavailable => {
                "Available when diri runs from the installed app.".to_owned()
            }
        }
    }
}

const APPROVAL_DETAIL: &str = "Allow diri in System Settings > General > Login Items to finish.";

pub(crate) struct LoginItem {
    backend: Box<dyn LoginItemBackend>,
}

impl LoginItem {
    pub(crate) fn new(backend: impl LoginItemBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
        }
    }

    /// The real thing in the app. Tests get a backend with nothing to
    /// register, so a settings view under test can never add a login item to
    /// the machine running it; reconciliation tests install a fake instead.
    pub(crate) fn system() -> Self {
        #[cfg(all(target_os = "macos", not(test)))]
        return Self::new(macos::MainApp);
        #[cfg(not(all(target_os = "macos", not(test))))]
        return Self::new(Unsupported);
    }

    /// The registration as it stands, for app start and for opening Settings.
    pub(crate) fn observe(&self) -> LoginItemState {
        LoginItemState {
            status: self.backend.status(),
            failure: None,
        }
    }

    /// Asks macOS for `wanted`, then reports the registration that resulted.
    /// A refusal is carried alongside the status it left behind.
    pub(crate) fn set(&self, wanted: bool) -> LoginItemState {
        if self.backend.status() == LoginItemStatus::Unavailable {
            return self.observe();
        }
        let failure = if wanted {
            self.backend.register()
        } else {
            self.backend.unregister()
        }
        .err();
        let status = self.backend.status();
        if wanted && status == LoginItemStatus::RequiresApproval {
            self.backend.open_approval_settings();
        }
        LoginItemState { status, failure }
    }
}

/// Platforms without a login-item service, and every test build.
#[cfg(not(all(target_os = "macos", not(test))))]
struct Unsupported;

#[cfg(not(all(target_os = "macos", not(test))))]
impl LoginItemBackend for Unsupported {
    fn status(&self) -> LoginItemStatus {
        LoginItemStatus::Unavailable
    }

    fn register(&self) -> Result<(), LoginItemError> {
        Err(LoginItemError { code: 0 })
    }

    fn unregister(&self) -> Result<(), LoginItemError> {
        Err(LoginItemError { code: 0 })
    }

    fn open_approval_settings(&self) {}
}

#[cfg(all(target_os = "macos", not(test)))]
mod macos {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::{NSBundle, NSError};

    use super::{LoginItemBackend, LoginItemError, LoginItemStatus};

    // `SMAppService` is reached by name, so nothing else pulls the framework
    // in; without this the class lookup below would always come back empty.
    #[link(name = "ServiceManagement", kind = "framework")]
    unsafe extern "C" {}

    /// `kSMErrorAlreadyRegistered`: the state asked for already holds.
    const ERROR_ALREADY_REGISTERED: isize = 12;
    /// `kSMErrorJobNotFound`: unregistering something that is not registered.
    const ERROR_JOB_NOT_FOUND: isize = 6;

    /// `SMAppService.mainApp`: the login item that launches this very bundle.
    pub(super) struct MainApp;

    /// `None` unless this process is the bundled app on macOS 13 or later. A
    /// bare `cargo run` binary has no bundle for the service to launch, and a
    /// side-by-side development bundle is not what the user means by "diri".
    fn service() -> Option<Retained<AnyObject>> {
        let bundle = NSBundle::mainBundle();
        let identifier = bundle.bundleIdentifier()?.to_string();
        if crate::dev_build::is_dev_bundle(&identifier)
            || !bundle.bundlePath().to_string().ends_with(".app")
        {
            return None;
        }
        let class = AnyClass::get(c"SMAppService")?;
        // SAFETY: `+[SMAppService mainAppService]` takes no arguments and
        // returns an autoreleased object, which `Retained` retains.
        unsafe { msg_send![class, mainAppService] }
    }

    fn outcome(result: Result<(), Retained<NSError>>, benign: isize) -> Result<(), LoginItemError> {
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.code() == benign => Ok(()),
            Err(error) => Err(LoginItemError { code: error.code() }),
        }
    }

    impl LoginItemBackend for MainApp {
        fn status(&self) -> LoginItemStatus {
            let Some(service) = service() else {
                return LoginItemStatus::Unavailable;
            };
            // SAFETY: `-[SMAppService status]` returns the NSInteger-backed
            // `SMAppServiceStatus`.
            let status: isize = unsafe { msg_send![&*service, status] };
            match status {
                1 => LoginItemStatus::Enabled,
                2 => LoginItemStatus::RequiresApproval,
                // `NotRegistered` (0), and `NotFound` (3), which the main app
                // reports before its first registration.
                _ => LoginItemStatus::NotRegistered,
            }
        }

        fn register(&self) -> Result<(), LoginItemError> {
            let service = service().ok_or(LoginItemError { code: 0 })?;
            // SAFETY: `-registerAndReturnError:` follows the Cocoa error
            // convention `msg_send!` models with the trailing `_`.
            let result = unsafe { msg_send![&*service, registerAndReturnError: _] };
            outcome(result, ERROR_ALREADY_REGISTERED)
        }

        fn unregister(&self) -> Result<(), LoginItemError> {
            let service = service().ok_or(LoginItemError { code: 0 })?;
            // SAFETY: as for `register`.
            let result = unsafe { msg_send![&*service, unregisterAndReturnError: _] };
            outcome(result, ERROR_JOB_NOT_FOUND)
        }

        fn open_approval_settings(&self) {
            if let Some(class) = AnyClass::get(c"SMAppService") {
                // SAFETY: `+[SMAppService openSystemSettingsLoginItems]` takes
                // no arguments and returns nothing.
                let _: () = unsafe { msg_send![class, openSystemSettingsLoginItems] };
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::{LoginItemBackend, LoginItemError, LoginItemStatus};

    /// A scriptable stand-in for macOS: it holds a registration, can refuse
    /// changes, and can hold new registrations for approval.
    #[derive(Clone)]
    pub(crate) struct Fake {
        state: Rc<RefCell<FakeState>>,
    }

    struct FakeState {
        status: LoginItemStatus,
        refuse: Option<LoginItemError>,
        hold_for_approval: bool,
        calls: Vec<&'static str>,
    }

    impl Fake {
        pub(crate) fn with_status(status: LoginItemStatus) -> Self {
            Self {
                state: Rc::new(RefCell::new(FakeState {
                    status,
                    refuse: None,
                    hold_for_approval: false,
                    calls: Vec::new(),
                })),
            }
        }

        pub(crate) fn refuse(&self, code: isize) {
            self.state.borrow_mut().refuse = Some(LoginItemError { code });
        }

        pub(crate) fn hold_for_approval(&self) {
            self.state.borrow_mut().hold_for_approval = true;
        }

        /// What the user did in System Settings behind the app's back.
        pub(crate) fn set_status(&self, status: LoginItemStatus) {
            self.state.borrow_mut().status = status;
        }

        /// Every mutating call the backend received, in order.
        pub(crate) fn calls(&self) -> Vec<&'static str> {
            self.state.borrow().calls.clone()
        }
    }

    impl LoginItemBackend for Fake {
        fn status(&self) -> LoginItemStatus {
            self.state.borrow().status
        }

        fn register(&self) -> Result<(), LoginItemError> {
            let mut state = self.state.borrow_mut();
            state.calls.push("register");
            if let Some(error) = state.refuse {
                return Err(error);
            }
            state.status = if state.hold_for_approval {
                LoginItemStatus::RequiresApproval
            } else {
                LoginItemStatus::Enabled
            };
            Ok(())
        }

        fn unregister(&self) -> Result<(), LoginItemError> {
            let mut state = self.state.borrow_mut();
            state.calls.push("unregister");
            if let Some(error) = state.refuse {
                return Err(error);
            }
            state.status = LoginItemStatus::NotRegistered;
            Ok(())
        }

        fn open_approval_settings(&self) {
            self.state.borrow_mut().calls.push("open_approval_settings");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Fake;
    use super::*;

    #[test]
    fn a_successful_toggle_reports_what_macos_now_holds() {
        let fake = Fake::with_status(LoginItemStatus::NotRegistered);
        let item = LoginItem::new(fake.clone());

        let on = item.set(true);
        assert!(on.enabled());
        assert!(!on.failed());
        assert!(on.preference(false));

        let off = item.set(false);
        assert!(!off.enabled());
        assert!(!off.preference(true));
        assert_eq!(fake.calls(), ["register", "unregister"]);
    }

    /// The bug this module exists for: a toggle that failed must not leave
    /// the preference, or the switch, claiming the app starts at login.
    #[test]
    fn a_refused_registration_does_not_claim_success() {
        let fake = Fake::with_status(LoginItemStatus::NotRegistered);
        fake.refuse(ERROR_INVALID_SIGNATURE);
        let item = LoginItem::new(fake.clone());

        let state = item.set(true);

        assert!(!state.enabled());
        assert!(!state.preference(true));
        assert!(state.failed());
        assert_eq!(
            state.detail(),
            "macOS only starts a signed copy of diri at login."
        );
    }

    #[test]
    fn a_refused_removal_keeps_showing_the_live_registration() {
        let fake = Fake::with_status(LoginItemStatus::Enabled);
        fake.refuse(2);
        let item = LoginItem::new(fake);

        let state = item.set(false);

        assert!(state.enabled());
        assert!(state.preference(false));
        assert_eq!(
            state.detail(),
            "macOS could not update Login Items (error 2)."
        );
    }

    #[test]
    fn a_registration_waiting_for_approval_is_off_and_points_at_system_settings() {
        let fake = Fake::with_status(LoginItemStatus::NotRegistered);
        fake.hold_for_approval();
        let item = LoginItem::new(fake.clone());

        let state = item.set(true);

        assert_eq!(state.status, LoginItemStatus::RequiresApproval);
        assert!(!state.enabled());
        assert!(!state.preference(true));
        assert!(!state.failed());
        assert!(
            state
                .detail()
                .contains("System Settings > General > Login Items")
        );
        assert_eq!(fake.calls(), ["register", "open_approval_settings"]);

        // Approved later, outside the app: the next look picks it up.
        fake.set_status(LoginItemStatus::Enabled);
        assert!(item.observe().preference(false));
    }

    #[test]
    fn an_unavailable_service_is_never_called_and_leaves_the_preference_alone() {
        let fake = Fake::with_status(LoginItemStatus::Unavailable);
        let item = LoginItem::new(fake.clone());

        let state = item.set(true);

        assert!(!state.available());
        assert!(!state.enabled());
        assert!(fake.calls().is_empty());
        // The installed app's registration is not this build's to overwrite.
        assert!(state.preference(true));
        assert!(!state.preference(false));
        assert_eq!(
            state.detail(),
            "Available when diri runs from the installed app."
        );
    }

    #[test]
    fn the_system_backend_in_tests_cannot_register_anything() {
        let state = LoginItem::system().set(true);
        assert_eq!(state.status, LoginItemStatus::Unavailable);
    }
}
