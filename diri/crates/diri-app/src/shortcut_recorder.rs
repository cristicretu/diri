//! What a key means while a shortcut is being recorded.
//!
//! Settings › Shortcuts and the command palette record shortcuts the same
//! way: Escape cancels, a bare ⌫ removes the shortcut, a combo with a modifier
//! (or a function key) is the new shortcut. When that combo already belongs
//! to another command, a bare ↩ takes it (Replace) and a bare ⇥ trades it for
//! this command's old shortcut (Swap). With no clash showing, ⇥ restores the
//! default. None of those bare keys can be a shortcut, so they never shadow a
//! combo someone wants to record.

use gpui::Keystroke;

use crate::commands::{ShortcutClash, ShortcutResolution};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecorderKey {
    Cancel,
    Remove,
    Reset,
    Resolve(ShortcutResolution),
    /// Keep listening: a modifier went down alone, or ↩/⇥ had nothing to
    /// resolve.
    Ignore,
    /// A plain key would type into every terminal; ask for a modifier.
    NeedsModifier,
    Record(String),
}

pub fn classify(key: &Keystroke, clash: Option<&ShortcutClash>) -> RecorderKey {
    let bare = !key.modifiers.modified();
    match key.key.as_str() {
        "escape" => return RecorderKey::Cancel,
        "shift" | "control" | "ctrl" | "alt" | "platform" | "function" | "fn" => {
            return RecorderKey::Ignore;
        }
        "backspace" | "delete" if bare => return RecorderKey::Remove,
        "enter" if bare => {
            return match clash {
                Some(clash) if clash.can_replace => {
                    RecorderKey::Resolve(ShortcutResolution::Replace)
                }
                _ => RecorderKey::Ignore,
            };
        }
        "tab" if bare => {
            return match clash {
                Some(clash) if clash.can_swap => RecorderKey::Resolve(ShortcutResolution::Swap),
                Some(_) => RecorderKey::Ignore,
                None => RecorderKey::Reset,
            };
        }
        _ => {}
    }
    let function_key = key
        .key
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=35).contains(&number));
    if bare && !function_key {
        RecorderKey::NeedsModifier
    } else {
        RecorderKey::Record(key.unparse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::CommandId;

    fn key(source: &str) -> Keystroke {
        Keystroke::parse(source).unwrap()
    }

    fn clash(can_swap: bool) -> ShortcutClash {
        ShortcutClash {
            binding: "cmd-k".into(),
            holder: CommandId::ToggleSidebar,
            can_replace: true,
            can_swap,
        }
    }

    #[test]
    fn bare_keys_steer_the_recorder_and_combos_are_recorded() {
        assert_eq!(classify(&key("escape"), None), RecorderKey::Cancel);
        assert_eq!(classify(&key("backspace"), None), RecorderKey::Remove);
        assert_eq!(classify(&key("tab"), None), RecorderKey::Reset);
        assert_eq!(classify(&key("enter"), None), RecorderKey::Ignore);
        assert_eq!(classify(&key("k"), None), RecorderKey::NeedsModifier);
        assert_eq!(
            classify(&key("f5"), None),
            RecorderKey::Record("f5".into())
        );
        // The platform's own chord (⌘ on macOS, Ctrl elsewhere), recorded
        // exactly as gpui spells it there.
        let chord = |source: &str| key(&crate::commands::test_chords(source));
        assert_eq!(
            classify(&chord("cmd-backspace"), None),
            RecorderKey::Record(chord("cmd-backspace").unparse())
        );
        assert_eq!(
            classify(&chord("cmd-enter"), Some(&clash(true))),
            RecorderKey::Record(chord("cmd-enter").unparse())
        );
    }

    #[test]
    fn a_clash_turns_return_into_replace_and_tab_into_swap_when_offered() {
        assert_eq!(
            classify(&key("enter"), Some(&clash(true))),
            RecorderKey::Resolve(ShortcutResolution::Replace)
        );
        assert_eq!(
            classify(&key("tab"), Some(&clash(true))),
            RecorderKey::Resolve(ShortcutResolution::Swap)
        );
        assert_eq!(
            classify(&key("tab"), Some(&clash(false))),
            RecorderKey::Ignore
        );
    }
}
