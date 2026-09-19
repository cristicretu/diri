//! macOS Secure Keyboard Entry, held only while the focused terminal is at a
//! password prompt.
//!
//! `EnableSecureEventInput` is reference counted per process, and while any
//! count is outstanding keystrokes are hidden from event taps system-wide:
//! text expanders, clipboard managers and accessibility tools stop seeing
//! keys in every app, not just this one. A leaked enable is therefore a bug
//! the user meets somewhere else entirely, so the count is owned by a lease
//! that can only end in the matching disable.

/// The operating system calls, behind a seam so the pairing can be tested
/// without Carbon.
pub(crate) trait SecureInputBackend {
    /// Takes one reference. `false` means the system refused and no
    /// reference is held, so none may be released.
    fn enable(&self) -> bool;
    /// Releases exactly one reference taken by a successful [`Self::enable`].
    fn disable(&self);
}

/// At most one outstanding reference, released on drop.
pub(crate) struct SecureInputLease {
    backend: Box<dyn SecureInputBackend>,
    held: bool,
}

impl SecureInputLease {
    pub(crate) fn new(backend: impl SecureInputBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
            held: false,
        }
    }

    /// The real thing in the app. Tests get a backend that does nothing, so
    /// a pane under test can never silence the keyboard of the machine
    /// running it; pairing tests install a recording backend instead.
    pub(crate) fn system() -> Self {
        #[cfg(all(target_os = "macos", not(test)))]
        return Self::new(macos::Carbon);
        #[cfg(not(all(target_os = "macos", not(test))))]
        return Self::new(Unsupported);
    }

    /// Makes the lease match `wanted`. Idempotent: the backend is called only
    /// on a change, which is what keeps enables and disables paired however
    /// often callers reconcile.
    pub(crate) fn set(&mut self, wanted: bool) {
        if wanted == self.held {
            return;
        }
        if wanted {
            self.held = self.backend.enable();
        } else {
            self.held = false;
            self.backend.disable();
        }
    }

    pub(crate) fn is_held(&self) -> bool {
        self.held
    }
}

impl Drop for SecureInputLease {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// Platforms without a secure-input facility, and every test build.
#[cfg(not(all(target_os = "macos", not(test))))]
struct Unsupported;

#[cfg(not(all(target_os = "macos", not(test))))]
impl SecureInputBackend for Unsupported {
    fn enable(&self) -> bool {
        false
    }

    fn disable(&self) {}
}

#[cfg(all(target_os = "macos", not(test)))]
mod macos {
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn EnableSecureEventInput() -> i32;
        fn DisableSecureEventInput() -> i32;
    }

    pub(super) struct Carbon;

    impl super::SecureInputBackend for Carbon {
        fn enable(&self) -> bool {
            // SAFETY: takes no arguments and only adjusts this process's
            // secure-input count; `noErr` is zero.
            unsafe { EnableSecureEventInput() == 0 }
        }

        fn disable(&self) {
            // SAFETY: as above. The lease calls this only after a successful
            // enable, so the count never goes below what this process took.
            let _ = unsafe { DisableSecureEventInput() };
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Counts what a real backend would have been asked to do.
    #[derive(Clone, Default)]
    pub(crate) struct Recorder {
        state: Rc<RefCell<RecorderState>>,
    }

    #[derive(Default)]
    struct RecorderState {
        enables: usize,
        disables: usize,
        refuse: bool,
    }

    impl Recorder {
        pub(crate) fn refusing() -> Self {
            let recorder = Self::default();
            recorder.state.borrow_mut().refuse = true;
            recorder
        }

        pub(crate) fn enables(&self) -> usize {
            self.state.borrow().enables
        }

        /// References the process would still hold: the number that must be
        /// zero whenever no focused terminal is at a password prompt.
        pub(crate) fn outstanding(&self) -> usize {
            let state = self.state.borrow();
            state.enables - state.disables
        }
    }

    impl super::SecureInputBackend for Recorder {
        fn enable(&self) -> bool {
            let mut state = self.state.borrow_mut();
            if state.refuse {
                return false;
            }
            state.enables += 1;
            true
        }

        fn disable(&self) {
            let mut state = self.state.borrow_mut();
            assert!(
                state.disables < state.enables,
                "disabled secure input without a matching enable"
            );
            state.disables += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Recorder;
    use super::*;

    #[test]
    fn repeated_reconciliation_takes_and_releases_exactly_one_reference() {
        let recorder = Recorder::default();
        let mut lease = SecureInputLease::new(recorder.clone());
        assert!(!lease.is_held());

        for _ in 0..3 {
            lease.set(true);
        }
        assert!(lease.is_held());
        assert_eq!(recorder.outstanding(), 1);

        for _ in 0..3 {
            lease.set(false);
        }
        assert!(!lease.is_held());
        assert_eq!(recorder.outstanding(), 0);

        lease.set(true);
        lease.set(false);
        assert_eq!(recorder.enables(), 2);
        assert_eq!(recorder.outstanding(), 0);
    }

    #[test]
    fn dropping_a_held_lease_releases_its_reference() {
        let recorder = Recorder::default();
        let mut lease = SecureInputLease::new(recorder.clone());
        lease.set(true);
        drop(lease);
        assert_eq!(recorder.outstanding(), 0);

        // Nothing to release: the recorder would panic on a stray disable.
        drop(SecureInputLease::new(recorder.clone()));
        assert_eq!(recorder.outstanding(), 0);
    }

    #[test]
    fn a_refused_enable_is_never_released() {
        let recorder = Recorder::refusing();
        let mut lease = SecureInputLease::new(recorder.clone());
        lease.set(true);
        assert!(!lease.is_held());
        lease.set(false);
        drop(lease);
        assert_eq!(recorder.outstanding(), 0);
    }

    #[test]
    fn two_leases_hold_independent_references() {
        // The system count is per process, so two panes never need to know
        // about each other: each releases only what it took.
        let recorder = Recorder::default();
        let mut first = SecureInputLease::new(recorder.clone());
        let mut second = SecureInputLease::new(recorder.clone());
        first.set(true);
        second.set(true);
        assert_eq!(recorder.outstanding(), 2);
        drop(first);
        assert_eq!(recorder.outstanding(), 1);
        second.set(false);
        assert_eq!(recorder.outstanding(), 0);
    }
}
