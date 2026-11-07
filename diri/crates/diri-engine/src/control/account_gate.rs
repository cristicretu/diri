//! Account switches and logins exclude every Agent launch.
//!
//! A launch that binds credentials while a Claude/Codex login is being
//! swapped starts on the old or a half-installed login. Dispatch used to
//! take this lock by method name, so any launch reached through another
//! method, or from a background thread, slipped past it. Every launch
//! primitive now asks for a [`LaunchPermit`] itself.
//!
//! The lock is never waited on: an operation that cannot have it fails with
//! a retryable error. The thread holding the exclusive side (the switch or
//! login itself) and a thread already holding a permit may launch again
//! without touching the lock, so the switch can relaunch its own tabs.
use std::cell::RefCell;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

thread_local! {
    /// Gates this thread currently holds, by address.
    static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

#[derive(Default)]
pub(super) struct AccountGate {
    lock: RwLock<()>,
}

/// Held by an account switch, login, or catalog change.
pub(super) struct AccountExclusive<'a> {
    _guard: RwLockWriteGuard<'a, ()>,
    _held: Held,
}

/// Held across a launch: from spec/credential binding until the Agent runs.
pub(super) struct LaunchPermit<'a> {
    _guard: Option<RwLockReadGuard<'a, ()>>,
    _held: Held,
}

struct Held(usize);

impl Held {
    fn push(key: usize) -> Self {
        HELD.with(|held| held.borrow_mut().push(key));
        Self(key)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(index) = held.iter().rposition(|key| *key == self.0) {
                held.remove(index);
            }
        });
    }
}

impl AccountGate {
    fn key(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    /// Whether this thread holds the gate in either mode.
    pub(super) fn held_here(&self) -> bool {
        let key = self.key();
        HELD.with(|held| held.borrow().contains(&key))
    }

    /// The switch/login side. `None` while any launch or other switch runs.
    pub(super) fn exclusive(&self) -> Option<AccountExclusive<'_>> {
        let guard = self.lock.try_write().ok()?;
        Some(AccountExclusive {
            _guard: guard,
            _held: Held::push(self.key()),
        })
    }

    /// The launch side. `None` while another thread switches accounts.
    pub(super) fn permit(&self) -> Option<LaunchPermit<'_>> {
        if self.held_here() {
            return Some(LaunchPermit {
                _guard: None,
                _held: Held::push(self.key()),
            });
        }
        let guard = self.lock.try_read().ok()?;
        Some(LaunchPermit {
            _guard: Some(guard),
            _held: Held::push(self.key()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_excludes_other_threads_but_not_its_own_relaunches() {
        let gate = AccountGate::default();
        let switch = gate.exclusive().expect("exclusive");
        // The switching thread relaunches its own tabs.
        assert!(gate.permit().is_some());
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert!(gate.permit().is_none(), "launch during a switch");
                    assert!(!gate.held_here());
                })
                .join()
                .unwrap();
        });
        drop(switch);
        assert!(!gate.held_here());
        let permit = gate.permit().expect("permit");
        let nested = gate.permit().expect("nested permit");
        assert!(gate.exclusive().is_none(), "switch during a launch");
        drop(permit);
        assert!(gate.held_here(), "nested permit still held");
        drop(nested);
        assert!(gate.exclusive().is_some());
    }
}
