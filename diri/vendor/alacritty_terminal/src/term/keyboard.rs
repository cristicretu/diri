//! Bounded keyboard state projection used by visible-grid acceleration caches.
//!
//! This does not change parser configuration or enable protocol negotiation.

use super::{KEYBOARD_MODE_STACK_MAX_DEPTH, KeyboardModes, Term, TermMode};

/// Both screen stacks and their current active flags. Construction validates
/// every bit and length before allocating; callers cannot construct invalid state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardSnapshot {
    current: u8,
    active: Vec<u8>,
    inactive: Vec<u8>,
}

impl KeyboardSnapshot {
    pub const MAX_BYTES: usize = 6 + 2 * KEYBOARD_MODE_STACK_MAX_DEPTH;

    pub fn current(&self) -> u8 {
        self.current
    }

    pub fn has_enhancements(&self) -> bool {
        self.current != 0
            || self
                .active
                .iter()
                .chain(&self.inactive)
                .any(|flags| *flags != 0)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(6 + self.active.len() + self.inactive.len());
        bytes.extend([1, self.current]);
        bytes.extend((self.active.len() as u16).to_le_bytes());
        bytes.extend((self.inactive.len() as u16).to_le_bytes());
        bytes.extend(&self.active);
        bytes.extend(&self.inactive);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !(6..=Self::MAX_BYTES).contains(&bytes.len()) || bytes[0] != 1 {
            return None;
        }
        let active = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
        let inactive = usize::from(u16::from_le_bytes([bytes[4], bytes[5]]));
        if active > KEYBOARD_MODE_STACK_MAX_DEPTH
            || inactive > KEYBOARD_MODE_STACK_MAX_DEPTH
            || bytes.len() != 6 + active + inactive
            || bytes[1] & !31 != 0
            || bytes[6..].iter().any(|flags| flags & !31 != 0)
            || bytes[1] != bytes[6..6 + active].last().copied().unwrap_or(0)
        {
            return None;
        }
        Some(Self {
            current: bytes[1],
            active: bytes[6..6 + active].to_vec(),
            inactive: bytes[6 + active..].to_vec(),
        })
    }
}

impl<T> Term<T> {
    pub fn keyboard_enhancements_enabled(&self) -> bool {
        self.config.kitty_keyboard
    }

    pub fn keyboard_snapshot(&self) -> KeyboardSnapshot {
        KeyboardSnapshot {
            current: KeyboardModes::from(self.mode).bits(),
            active: self
                .keyboard_mode_stack
                .iter()
                .map(|flags| flags.bits())
                .collect(),
            inactive: self
                .inactive_keyboard_mode_stack
                .iter()
                .map(|flags| flags.bits())
                .collect(),
        }
    }

    /// A cache may not activate a disabled protocol. Incompatible caches fall
    /// back to the caller's existing raw-log recovery, with unknown input state.
    pub fn can_restore_keyboard_snapshot(&self, snapshot: &KeyboardSnapshot) -> bool {
        self.config.kitty_keyboard || !snapshot.has_enhancements()
    }

    pub fn restore_keyboard_snapshot(&mut self, snapshot: &KeyboardSnapshot) -> bool {
        if !self.can_restore_keyboard_snapshot(snapshot) {
            return false;
        }
        self.keyboard_mode_stack = snapshot
            .active
            .iter()
            .map(|flags| KeyboardModes::from_bits_retain(*flags))
            .collect();
        self.inactive_keyboard_mode_stack = snapshot
            .inactive
            .iter()
            .map(|flags| KeyboardModes::from_bits_retain(*flags))
            .collect();
        self.mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
        self.mode
            .insert(KeyboardModes::from_bits_retain(snapshot.current).into());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::VoidListener;
    use crate::term::Config;
    use crate::term::test::TermSize;
    use crate::vte::ansi::{Handler, KeyboardModesApplyBehavior};

    #[test]
    fn snapshot_restores_both_stacks_and_later_pop_without_activating_configuration() {
        let config = Config {
            kitty_keyboard: true,
            ..Config::default()
        };
        let mut term = Term::new(config.clone(), &TermSize::new(5, 10), VoidListener);
        term.push_keyboard_mode(KeyboardModes::from_bits_retain(5));
        term.push_keyboard_mode(KeyboardModes::from_bits_retain(7));
        term.swap_alt();
        term.push_keyboard_mode(KeyboardModes::from_bits_retain(8));
        Handler::set_keyboard_mode(
            &mut term,
            KeyboardModes::from_bits_retain(16),
            KeyboardModesApplyBehavior::Union,
        );
        let snapshot = term.keyboard_snapshot();
        assert_eq!(snapshot.current(), 24);
        let decoded = KeyboardSnapshot::decode(&snapshot.encode()).unwrap();
        assert_eq!(decoded, snapshot);
        let mut restored = Term::new(config, &TermSize::new(5, 10), VoidListener);
        restored.swap_alt();
        assert!(restored.restore_keyboard_snapshot(&decoded));
        restored.swap_alt();
        assert_eq!(restored.keyboard_snapshot().current(), 7);
        restored.pop_keyboard_modes(1);
        assert_eq!(restored.keyboard_snapshot().current(), 5);
        restored.swap_alt();
        assert_eq!(restored.keyboard_snapshot().current(), 24);
        let mut disabled = Term::new(Config::default(), &TermSize::new(5, 10), VoidListener);
        assert!(!disabled.restore_keyboard_snapshot(&decoded));
        assert_eq!(disabled.keyboard_snapshot().current(), 0);
    }

    #[test]
    fn snapshot_rejects_unknown_bits_lengths_versions_and_inconsistent_active_flags() {
        assert!(KeyboardSnapshot::decode(&[1, 0, 0, 0, 0, 0]).is_some());
        for invalid in [
            vec![2, 0, 0, 0, 0, 0],
            vec![1, 32, 1, 0, 0, 0, 32],
            vec![1, 2, 1, 0, 0, 0, 1],
            vec![1, 0, 0, 0, 0, 16],
            vec![1, 0, 0, 0, 0, 0, 0],
            vec![1; KeyboardSnapshot::MAX_BYTES + 1],
        ] {
            assert!(KeyboardSnapshot::decode(&invalid).is_none());
        }
        let mut maximum = vec![1, 1, 0, 16, 0, 16];
        maximum.resize(KeyboardSnapshot::MAX_BYTES, 1);
        assert_eq!(
            KeyboardSnapshot::decode(&maximum).unwrap().encode(),
            maximum
        );
        let mut excessive_stack = vec![1, 0, 1, 16, 0, 0];
        excessive_stack.resize(6 + 4097, 0);
        assert!(KeyboardSnapshot::decode(&excessive_stack).is_none());
        for bits in 0..32 {
            assert_eq!(
                KeyboardSnapshot::decode(&[1, bits, 1, 0, 0, 0, bits])
                    .unwrap()
                    .current(),
                bits
            );
        }
    }
}
