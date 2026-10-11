//! "Default terminal": whether macOS sends `ssh://`, `telnet://` and
//! `x-man-page://` links and `.command` files to diri, and the one button
//! that makes it so.
//!
//! Launch Services owns the answer, and the user can change it elsewhere at
//! any time, so Settings reads it back every time it opens. diri never takes
//! the role on its own: only [`DefaultTerminal::make_default`] changes it,
//! and only a click on Settings' button calls that.

use crate::i18n::{t, tf};

/// The URL schemes diri declares in `assets/Info.plist`.
pub(crate) const URL_SCHEMES: [&str; 3] = ["ssh", "telnet", "x-man-page"];
/// Terminal's type for `.command` and `.tool` files. Plain `.sh` files stay
/// with whatever edits them; diri only appears under Open With for those.
pub(crate) const CONTENT_TYPES: [&str; 1] = ["com.apple.terminal.shell-script"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefaultTerminalStatus {
    /// Every scheme and type above opens in diri.
    Default,
    // Only the macOS backend (and tests) reads this from the system.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    NotDefault,
    /// Not the installed app (a `cargo run` binary, a test, a development
    /// bundle), or not macOS.
    Unavailable,
}

/// The system calls, behind a seam: each is a synchronous round trip to
/// Launch Services, so callers run them off the UI thread, and tests use a
/// backend that cannot change the machine's defaults.
pub(crate) trait DefaultTerminalBackend: Send + Sync {
    /// The bundle that opens `scheme` links, if any.
    fn url_handler(&self, scheme: &str) -> Option<String>;
    fn content_handler(&self, content_type: &str) -> Option<String>;
    fn set_url_handler(&self, scheme: &str) -> Result<(), i32>;
    fn set_content_handler(&self, content_type: &str) -> Result<(), i32>;
    /// This app's bundle identifier, when it is one that can be the default.
    fn bundle_identifier(&self) -> Option<String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DefaultTerminalState {
    pub(crate) status: DefaultTerminalStatus,
    /// The `OSStatus` of the change that was just refused.
    pub(crate) failure: Option<i32>,
}

impl DefaultTerminalState {
    pub(crate) const UNKNOWN: Self = Self {
        status: DefaultTerminalStatus::Unavailable,
        failure: None,
    };

    pub(crate) fn can_make_default(self) -> bool {
        self.status == DefaultTerminalStatus::NotDefault
    }

    pub(crate) fn failed(self) -> bool {
        self.failure.is_some()
    }

    pub(crate) fn detail(self) -> String {
        if let Some(code) = self.failure {
            return tf("default_terminal.failed", &[("code", &code)]);
        }
        match self.status {
            DefaultTerminalStatus::Default => t("default_terminal.is_default").to_owned(),
            DefaultTerminalStatus::NotDefault => t("default_terminal.not_default").to_owned(),
            DefaultTerminalStatus::Unavailable => t("default_terminal.unavailable").to_owned(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct DefaultTerminal {
    backend: std::sync::Arc<dyn DefaultTerminalBackend>,
}

impl DefaultTerminal {
    pub(crate) fn new(backend: impl DefaultTerminalBackend + 'static) -> Self {
        Self {
            backend: std::sync::Arc::new(backend),
        }
    }

    /// Launch Services in the app; nothing at all in tests, so no test can
    /// change which app opens the machine's `ssh://` links.
    pub(crate) fn system() -> Self {
        #[cfg(all(target_os = "macos", not(test)))]
        return Self::new(macos::LaunchServices);
        #[cfg(not(all(target_os = "macos", not(test))))]
        return Self::new(Unsupported);
    }

    pub(crate) fn observe(&self) -> DefaultTerminalState {
        let Some(bundle) = self.backend.bundle_identifier() else {
            return DefaultTerminalState::UNKNOWN;
        };
        let ours = |handler: Option<String>| {
            handler.is_some_and(|handler| handler.eq_ignore_ascii_case(&bundle))
        };
        let default = URL_SCHEMES
            .iter()
            .all(|scheme| ours(self.backend.url_handler(scheme)))
            && CONTENT_TYPES
                .iter()
                .all(|content_type| ours(self.backend.content_handler(content_type)));
        DefaultTerminalState {
            status: if default {
                DefaultTerminalStatus::Default
            } else {
                DefaultTerminalStatus::NotDefault
            },
            failure: None,
        }
    }

    /// Makes diri the handler for every scheme and type, then reports what
    /// Launch Services now says. Called only from the Settings button.
    pub(crate) fn make_default(&self) -> DefaultTerminalState {
        if self.backend.bundle_identifier().is_none() {
            return DefaultTerminalState::UNKNOWN;
        }
        let mut failure = None;
        for scheme in URL_SCHEMES {
            if let Err(code) = self.backend.set_url_handler(scheme) {
                failure.get_or_insert(code);
            }
        }
        for content_type in CONTENT_TYPES {
            if let Err(code) = self.backend.set_content_handler(content_type) {
                failure.get_or_insert(code);
            }
        }
        DefaultTerminalState {
            failure,
            ..self.observe()
        }
    }
}

#[cfg(not(all(target_os = "macos", not(test))))]
struct Unsupported;

#[cfg(not(all(target_os = "macos", not(test))))]
impl DefaultTerminalBackend for Unsupported {
    fn url_handler(&self, _: &str) -> Option<String> {
        None
    }

    fn content_handler(&self, _: &str) -> Option<String> {
        None
    }

    fn set_url_handler(&self, _: &str) -> Result<(), i32> {
        Err(0)
    }

    fn set_content_handler(&self, _: &str) -> Result<(), i32> {
        Err(0)
    }

    fn bundle_identifier(&self) -> Option<String> {
        None
    }
}

#[cfg(all(target_os = "macos", not(test)))]
mod macos {
    use std::ffi::c_void;

    use objc2::rc::{Retained, autoreleasepool};
    use objc2_foundation::{NSBundle, NSString};

    use super::DefaultTerminalBackend;

    type CFStringRef = *const c_void;
    /// `kLSRolesAll`
    const ROLES_ALL: u32 = 0xFFFF_FFFF;

    // Deprecated in name only: these remain the synchronous Launch Services
    // calls, and their `NSWorkspace` replacements need a UTType for files.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, handler: CFStringRef) -> i32;
        fn LSCopyDefaultRoleHandlerForContentType(kind: CFStringRef, role: u32) -> CFStringRef;
        fn LSSetDefaultRoleHandlerForContentType(
            kind: CFStringRef,
            role: u32,
            handler: CFStringRef,
        ) -> i32;
    }

    pub(super) struct LaunchServices;

    fn cf(string: &NSString) -> CFStringRef {
        // NSString and CFString are toll-free bridged.
        std::ptr::from_ref(string).cast()
    }

    /// Takes ownership of a `Copy`-rule CFString.
    fn owned(string: CFStringRef) -> Option<String> {
        // SAFETY: the `LSCopy…` functions return a +1 CFString or NULL, and
        // `from_raw` releases it when dropped.
        let string = unsafe { Retained::from_raw(string.cast_mut().cast::<NSString>()) }?;
        Some(string.to_string())
    }

    fn status(code: i32) -> Result<(), i32> {
        if code == 0 { Ok(()) } else { Err(code) }
    }

    impl DefaultTerminalBackend for LaunchServices {
        fn url_handler(&self, scheme: &str) -> Option<String> {
            autoreleasepool(|_| {
                let scheme = NSString::from_str(scheme);
                // SAFETY: a valid CFString in, a +1 CFString or NULL out.
                owned(unsafe { LSCopyDefaultHandlerForURLScheme(cf(&scheme)) })
            })
        }

        fn content_handler(&self, content_type: &str) -> Option<String> {
            autoreleasepool(|_| {
                let content_type = NSString::from_str(content_type);
                // SAFETY: as for `url_handler`.
                owned(unsafe {
                    LSCopyDefaultRoleHandlerForContentType(cf(&content_type), ROLES_ALL)
                })
            })
        }

        fn set_url_handler(&self, scheme: &str) -> Result<(), i32> {
            let bundle = self.bundle_identifier().ok_or(0)?;
            autoreleasepool(|_| {
                let scheme = NSString::from_str(scheme);
                let bundle = NSString::from_str(&bundle);
                // SAFETY: two valid CFStrings; returns an OSStatus.
                status(unsafe { LSSetDefaultHandlerForURLScheme(cf(&scheme), cf(&bundle)) })
            })
        }

        fn set_content_handler(&self, content_type: &str) -> Result<(), i32> {
            let bundle = self.bundle_identifier().ok_or(0)?;
            autoreleasepool(|_| {
                let content_type = NSString::from_str(content_type);
                let bundle = NSString::from_str(&bundle);
                // SAFETY: as for `set_url_handler`.
                status(unsafe {
                    LSSetDefaultRoleHandlerForContentType(cf(&content_type), ROLES_ALL, cf(&bundle))
                })
            })
        }

        /// Only the installed app: a `cargo run` binary has no bundle to
        /// launch, and a side-by-side development bundle is not "diri".
        fn bundle_identifier(&self) -> Option<String> {
            autoreleasepool(|_| {
                let bundle = NSBundle::mainBundle();
                let identifier = bundle.bundleIdentifier()?.to_string();
                (!crate::dev_build::is_dev_bundle(&identifier)
                    && bundle.bundlePath().to_string().ends_with(".app"))
                .then_some(identifier)
            })
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::DefaultTerminalBackend;

    /// Launch Services in memory: a bundle id, the current handlers, and an
    /// optional refusal.
    #[derive(Clone, Default)]
    pub(crate) struct Fake {
        pub(crate) bundle: Option<String>,
        pub(crate) handlers: Arc<Mutex<HashMap<String, String>>>,
        pub(crate) refuse: Option<i32>,
        pub(crate) calls: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        pub(crate) fn installed() -> Self {
            Self {
                bundle: Some("com.dirijor.diri".into()),
                ..Self::default()
            }
        }

        pub(crate) fn hand(&self, key: &str, bundle: &str) {
            self.handlers
                .lock()
                .unwrap()
                .insert(key.into(), bundle.into());
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn set(&self, key: &str) -> Result<(), i32> {
            self.calls.lock().unwrap().push(key.into());
            if let Some(code) = self.refuse {
                return Err(code);
            }
            let bundle = self.bundle.clone().ok_or(0)?;
            self.hand(key, &bundle);
            Ok(())
        }
    }

    impl DefaultTerminalBackend for Fake {
        fn url_handler(&self, scheme: &str) -> Option<String> {
            self.handlers.lock().unwrap().get(scheme).cloned()
        }

        fn content_handler(&self, content_type: &str) -> Option<String> {
            self.handlers.lock().unwrap().get(content_type).cloned()
        }

        fn set_url_handler(&self, scheme: &str) -> Result<(), i32> {
            self.set(scheme)
        }

        fn set_content_handler(&self, content_type: &str) -> Result<(), i32> {
            self.set(content_type)
        }

        fn bundle_identifier(&self) -> Option<String> {
            self.bundle.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Fake;
    use super::*;

    #[test]
    fn terminal_app_as_handler_is_not_default_and_observing_changes_nothing() {
        let fake = Fake::installed();
        fake.hand("ssh", "com.apple.Terminal");
        let state = DefaultTerminal::new(fake.clone()).observe();
        assert_eq!(state.status, DefaultTerminalStatus::NotDefault);
        assert!(state.can_make_default());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn make_default_claims_every_scheme_and_type() {
        let fake = Fake::installed();
        let terminal = DefaultTerminal::new(fake.clone());
        let state = terminal.make_default();
        assert_eq!(state.status, DefaultTerminalStatus::Default);
        assert!(!state.can_make_default());
        assert_eq!(
            fake.calls(),
            [
                "ssh",
                "telnet",
                "x-man-page",
                "com.apple.terminal.shell-script"
            ]
        );
        // Bundle ids compare case-insensitively, as Launch Services does.
        fake.hand("ssh", "COM.DIRIJOR.DIRI");
        assert_eq!(terminal.observe().status, DefaultTerminalStatus::Default);
    }

    #[test]
    fn one_scheme_elsewhere_means_not_default() {
        let fake = Fake::installed();
        let terminal = DefaultTerminal::new(fake.clone());
        terminal.make_default();
        fake.hand("x-man-page", "com.apple.Terminal");
        assert_eq!(terminal.observe().status, DefaultTerminalStatus::NotDefault);
    }

    #[test]
    fn a_refusal_is_reported_with_the_status_it_left() {
        let fake = Fake {
            refuse: Some(-54),
            ..Fake::installed()
        };
        let state = DefaultTerminal::new(fake).make_default();
        assert_eq!(state.status, DefaultTerminalStatus::NotDefault);
        assert!(state.failed());
        assert!(state.detail().contains("-54"));
    }

    #[test]
    fn without_an_installed_bundle_nothing_is_changed() {
        let fake = Fake::default();
        let state = DefaultTerminal::new(fake.clone()).make_default();
        assert_eq!(state.status, DefaultTerminalStatus::Unavailable);
        assert!(!state.can_make_default());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn the_system_backend_in_tests_cannot_change_defaults() {
        let state = DefaultTerminal::system().make_default();
        assert_eq!(state, DefaultTerminalState::UNKNOWN);
    }
}
