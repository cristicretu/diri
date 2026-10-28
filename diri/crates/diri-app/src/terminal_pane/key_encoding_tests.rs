//! Key encoding audit: macOS key-downs as GPUI delivers them on a US layout,
//! through the pane's adapter and the shared encoder, against reference bytes
//! recorded from a reference terminal multiplexer whose server encodes W3C key
//! events (xterm legacy rules plus the kitty keyboard protocol).

use super::*;
use diri_term::keys::{KeyAction, KeyboardState};
use gpui::{KeyDownEvent, Keystroke, Modifiers};

/// US-layout unshifted and shifted characters for the keys the audit covers.
const US_SHIFTED: &[(char, char)] = &[
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('6', '^'),
    ('8', '*'),
    ('0', ')'),
    ('/', '?'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    ('-', '_'),
    ('=', '+'),
    ('\'', '"'),
    (';', ':'),
    (',', '<'),
    ('.', '>'),
    ('`', '~'),
];

/// One combo in the reference's binding syntax (`ctrl+shift+[KeyA]`, `alt+up`)
/// as the GPUI macOS key-down a US keyboard produces, and its native key code
/// when it is a keypad key.
fn us_key_down(combo: &str) -> (KeyDownEvent, Option<Native>) {
    let mut parts: Vec<&str> = combo.split('+').collect();
    let key = parts.pop().unwrap();
    let mut modifiers = Modifiers::default();
    for part in parts {
        match part {
            "shift" => modifiers.shift = true,
            "ctrl" => modifiers.control = true,
            "alt" => modifiers.alt = true,
            "super" => modifiers.platform = true,
            other => panic!("modifier {other}"),
        }
    }
    let typed = !modifiers.control && !modifiers.platform;
    let named = |name: &str, key_char: Option<&str>| Keystroke {
        modifiers,
        key: name.to_owned(),
        key_char: key_char.map(str::to_owned),
    };
    let mut native = None;
    let keystroke = match key {
        "page_up" => named("pageup", None),
        "page_down" => named("pagedown", None),
        "space" => named("space", Some(" ")),
        "tab" => named("tab", Some("\t")),
        "enter" => named("enter", Some("\n")),
        "up" | "down" | "left" | "right" | "home" | "end" | "insert" | "delete" | "backspace"
        | "escape" => named(key, None),
        f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => named(f, None),
        physical => {
            let code = physical
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .unwrap_or_else(|| panic!("key {physical}"));
            if let Some(pad) = code.strip_prefix("Numpad") {
                let (ch, code) = match pad {
                    "0" => ("0", 0x52),
                    "1" => ("1", 0x53),
                    "5" => ("5", 0x57),
                    "Decimal" => (".", 0x41),
                    "Divide" => ("/", 0x4b),
                    "Multiply" => ("*", 0x43),
                    "Subtract" => ("-", 0x4e),
                    "Add" => ("+", 0x45),
                    "Equal" => ("=", 0x51),
                    "Enter" => ("enter", 0x4c),
                    other => panic!("numpad {other}"),
                };
                native = Some(Native::Keypad(code));
                if ch == "enter" {
                    named("enter", Some("\n"))
                } else {
                    named(ch, typed.then_some(ch))
                }
            } else {
                let base = if let Some(letter) = code.strip_prefix("Key") {
                    letter.to_ascii_lowercase().chars().next().unwrap()
                } else if let Some(digit) = code.strip_prefix("Digit") {
                    digit.chars().next().unwrap()
                } else {
                    match code {
                        "Slash" => '/',
                        "BracketLeft" => '[',
                        "BracketRight" => ']',
                        "Backslash" => '\\',
                        "Minus" => '-',
                        "Equal" => '=',
                        "Quote" => '\'',
                        "Semicolon" => ';',
                        "Comma" => ',',
                        "Period" => '.',
                        "Backquote" => '`',
                        other => panic!("code {other}"),
                    }
                };
                let shifted = if base.is_ascii_lowercase() {
                    base.to_ascii_uppercase()
                } else {
                    US_SHIFTED.iter().find(|(b, _)| *b == base).unwrap().1
                };
                // GPUI keeps Shift only for letters; other shifted keys
                // arrive resolved with Shift cleared.
                let mut mods = modifiers;
                let logical = if base.is_ascii_lowercase() || !modifiers.shift {
                    base
                } else {
                    mods.shift = false;
                    shifted
                };
                let produced = if modifiers.shift { shifted } else { base };
                native = Some(Native::Layout(base, shifted));
                // Option composes a non-ASCII character on US (∫ for B).
                let key_char = typed.then(|| {
                    if modifiers.alt {
                        "\u{222b}".to_owned()
                    } else {
                        produced.to_string()
                    }
                });
                Keystroke {
                    modifiers: mods,
                    key: logical.to_string(),
                    key_char,
                }
            }
        }
    };
    (
        KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        },
        native,
    )
}

/// Native key-down metadata GPUI exposes during dispatch.
#[derive(Clone, Copy)]
enum Native {
    /// A keypad key's hardware code.
    Keypad(u16),
    /// A character key's unshifted and shifted layout characters.
    Layout(char, char),
}

/// What the pane writes for one key, mirroring `handle_key_down` after
/// overlays (Command chords other than Backspace go to navigation or the
/// app), and for a release, `send_key_release` with the press remembered.
fn pane_bytes(
    event: &KeyDownEvent,
    native: Option<Native>,
    keyboard: Option<KeyboardState>,
    action: KeyAction,
) -> Result<Vec<u8>, String> {
    let run = || {
        if event.keystroke.modifiers.platform && event.keystroke.key != "backspace" {
            return Ok(terminal_command_navigation(&event.keystroke)
                .map(<[u8]>::to_vec)
                .unwrap_or_default());
        }
        let mapped =
            terminal_key_event(event, OptionKey::Meta).ok_or_else(|| "unmapped".to_owned())?;
        let (pressed, mut modifiers) = kitty_key_event(event, keyboard, mapped);
        let mut held = Vec::new();
        remember_held_key(&mut held, &event.keystroke, &pressed, modifiers.shift);
        let term_event = if action == KeyAction::Release {
            let (released, shift) = take_held_key(&mut held, &event.keystroke).unwrap();
            let (_, release_modifiers) =
                terminal_keystroke_event(&event.keystroke, OptionKey::Meta).unwrap();
            modifiers = release_modifiers;
            modifiers.shift |= shift;
            released
        } else {
            pressed
        };
        diri_term::keys::encode_interactive_action(&term_event, modifiers, keyboard, action)
            .map_err(|error| error.to_string())
    };
    match native {
        Some(Native::Keypad(code)) => gpui_macos::with_native_key_for_test(event.clone(), code, run),
        Some(Native::Layout(unshifted, shifted)) => gpui_macos::with_native_key_layout_for_test(
            event.clone(),
            0,
            (&unshifted.to_string(), &shifted.to_string()),
            run,
        ),
        None => run(),
    }
}

fn mode_state(mode: &str) -> Option<KeyboardState> {
    let mut state = KeyboardState {
        enhancements: Some(0.try_into().unwrap()),
        ..Default::default()
    };
    for part in mode.split('_') {
        match part {
            "normal" | "mok1" | "mok2" => {}
            "decckm" | "ckm" => state.application_cursor_keys = true,
            "deckpam" | "kpam" => state.application_keypad = true,
            kitty if kitty.starts_with('k') => {
                state.enhancements = Some(kitty[1..].parse::<u8>().unwrap().try_into().unwrap());
            }
            other => panic!("mode {other}"),
        }
    }
    Some(state)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes recorded from the reference server for real key events on an
/// isolated session whose program logged raw input (mode, key in the
/// reference's binding syntax, bytes). Every row goes through the pane's GPUI
/// adapter and the shared encoder exactly as a US-layout key-down would.
const REFERENCE_PRESSES: &[(&str, &str, &[u8])] = &[
    ("normal", "ctrl+space", b"\x00"),
    ("normal", "ctrl+alt+space", b"\x1b\x00"),
    ("normal", "ctrl+[Digit2]", b"\x00"),
    ("normal", "ctrl+[Digit3]", b"\x1b"),
    ("normal", "ctrl+[Digit6]", b"\x1e"),
    ("normal", "ctrl+[Digit8]", b"\x7f"),
    ("normal", "ctrl+[Slash]", b"\x1f"),
    ("normal", "ctrl+[KeyU]", b"\x15"),
    ("normal", "ctrl+[BracketRight]", b"\x1d"),
    ("normal", "ctrl+[Backslash]", b"\x1c"),
    ("normal", "ctrl+alt+[KeyA]", b"\x1b\x01"),
    ("normal", "ctrl+alt+[KeyJ]", b"\x1b\x0a"),
    ("normal", "ctrl+alt+[BracketRight]", b"\x1b\x1d"),
    ("normal", "ctrl+alt+[Slash]", b"\x1b\x1f"),
    ("normal", "ctrl+alt+[Digit2]", b"\x1b\x00"),
    ("normal", "ctrl+backspace", b"\x08"),
    ("normal", "ctrl+shift+backspace", b"\x08"),
    ("normal", "ctrl+alt+backspace", b"\x1b\x08"),
    ("normal", "backspace", b"\x7f"),
    ("normal", "alt+backspace", b"\x1b\x7f"),
    ("normal", "alt+left", b"\x1b[1;3D"),
    ("normal", "alt+right", b"\x1b[1;3C"),
    ("normal", "ctrl+left", b"\x1b[1;5D"),
    ("normal", "shift+tab", b"\x1b[Z"),
    ("normal", "alt+[KeyB]", b"\x1bb"),
    ("normal", "alt+enter", b"\x1b\x0d"),
    ("normal", "f13", b"\x1b[25~"),
    ("normal", "f20", b"\x1b[34~"),
    ("normal", "f21", b"\x1b[42~"),
    ("normal", "f24", b"\x1b[45~"),
    ("normal", "shift+f13", b"\x1b[25;2~"),
    ("normal", "ctrl+f13", b"\x1b[25;5~"),
    ("normal", "ctrl+alt+f24", b"\x1b[45;7~"),
    ("normal", "f5", b"\x1b[15~"),
    ("normal", "ctrl+up", b"\x1b[1;5A"),
    ("decckm", "up", b"\x1bOA"),
    ("decckm", "shift+up", b"\x1b[1;2A"),
    ("decckm", "home", b"\x1bOH"),
    ("decckm", "alt+left", b"\x1b[1;3D"),
    ("deckpam", "[Numpad0]", b"0"),
    ("deckpam", "[Numpad5]", b"5"),
    ("deckpam", "[NumpadEnter]", b"\x0d"),
    ("deckpam", "[NumpadDecimal]", b"."),
    ("deckpam", "shift+[Numpad5]", b"5"),
    ("deckpam", "ctrl+[NumpadAdd]", b"+"),
    ("ckm_kpam", "up", b"\x1bOA"),
    ("ckm_kpam", "[Numpad5]", b"5"),
    ("k1", "ctrl+shift+[Digit2]", b"\x1b[50;6u"),
    ("k1", "ctrl+shift+[Slash]", b"\x1b[47;6u"),
    ("k1", "alt+shift+[Slash]", b"\x1b[47;4u"),
    ("k1", "ctrl+alt+shift+[Minus]", b"\x1b[45;8u"),
    ("k1", "ctrl+shift+[KeyA]", b"\x1b[97;6u"),
    ("k1", "shift+[Digit2]", b"@"),
    ("k1", "shift+enter", b"\x1b[13;2u"),
    ("k1", "ctrl+enter", b"\x1b[13;5u"),
    ("k1", "alt+backspace", b"\x1b[127;3u"),
    ("k1", "ctrl+backspace", b"\x1b[127;5u"),
    ("k1", "escape", b"\x1b[27u"),
    ("k1", "ctrl+space", b"\x1b[32;5u"),
    ("k1", "alt+[KeyB]", b"\x1b[98;3u"),
    ("k1", "f13", b"\x1b[57376u"),
    ("k1", "ctrl+f24", b"\x1b[57387;5u"),
    ("k1", "f3", b"\x1b[13~"),
    ("k1", "[Numpad5]", b"\x1b[57404u"),
    ("k1_ckm", "up", b"\x1b[A"),
    ("k1_ckm", "ctrl+shift+[Digit6]", b"\x1b[54;6u"),
    ("k8", "shift+[Digit2]", b"\x1b[50;2u"),
    ("k8", "shift+[Slash]", b"\x1b[47;2u"),
    ("k8", "shift+[KeyA]", b"\x1b[97;2u"),
    ("k8", "[KeyA]", b"\x1b[97u"),
    ("k8", "enter", b"\x1b[13u"),
    ("k8", "f13", b"\x1b[57376u"),
    ("k24", "ctrl+alt+shift+[KeyA]", b"\x1b[97;8u"),
    ("k24", "alt+space", b"\x1b[32;3u"),
    ("k24", "ctrl+[KeyA]", b"\x1b[97;5u"),
    ("k24", "shift+[KeyA]", b"\x1b[97;2;65u"),
];

/// Releases and repeats, for programs that asked for kitty event types.
const REFERENCE_RELEASES: &[(&str, &str, &[u8])] = &[
    ("k3", "[KeyA]", b"\x1b[97;1:3u"),
    ("k3", "ctrl+[KeyA]", b"\x1b[97;5:3u"),
    ("k3", "shift+[KeyA]", b"\x1b[97;2:3u"),
    ("k3", "[Numpad1]", b"\x1b[57400;1:3u"),
    ("k3", "f13", b"\x1b[57376;1:3u"),
    ("k3", "enter", b""),
    ("k3", "escape", b"\x1b[27;1:3u"),
    ("k11", "enter", b"\x1b[13;1:3u"),
    ("k11", "[Numpad1]", b"\x1b[57400;1:3u"),
    ("k11", "f13", b"\x1b[57376;1:3u"),
    ("k15", "shift+[KeyA]#repeat", b"\x1b[97:65;2:2u"),
];

/// Where Diri deliberately differs from the reference, with the reason.
const INTENTIONAL_DIFFERENCES: &[(&str, &str, &[u8])] = &[
    // Legacy Shift-Enter stays CSI u (the reference sends xterm's
    // modifyOtherKeys form `CSI 27;2;13~`): Claude Code reads CSI 13;2u as a
    // newline without negotiating anything.
    ("normal", "shift+enter", b"\x1b[13;2u"),
    // Legacy Ctrl-Enter stays Return, as xterm sends it; programs that want
    // it distinct negotiate kitty flags (then it is CSI 13;5u, as above).
    ("normal", "ctrl+enter", b"\r"),
    // Modified F3 keeps xterm's `CSI 1;mod R`, which xterm-256color's
    // terminfo (kf15) expects; the reference sends `CSI 13;mod ~`.
    ("normal", "shift+f3", b"\x1b[1;2R"),
    // Legacy Ctrl-Shift-letter stays the C0 byte, as xterm sends it; the
    // reference sends CSI u, which unnegotiated programs cannot read.
    ("normal", "ctrl+shift+[KeyA]", b"\x01"),
    // Option is Meta on keypad keys too (the reference ignores it).
    ("normal", "alt+[Numpad5]", b"\x1b5"),
    // Command chords are macOS line editing, not a Super modifier.
    ("normal", "super+backspace", b"\x15"),
    ("normal", "super+left", b"\x01"),
    ("k1", "super+right", b"\x05"),
    // With alternate keys, Shift reports the shifted key even with Ctrl or
    // Alt held, as the kitty spec says; the reference drops it there.
    ("k5", "ctrl+shift+[Digit2]", b"\x1b[50:64;6u"),
];

#[test]
fn key_encoding_matches_the_reference_where_it_is_right() {
    let mut failures = Vec::new();
    for (table, action) in [
        (REFERENCE_PRESSES, None),
        (REFERENCE_RELEASES, Some(KeyAction::Release)),
        (INTENTIONAL_DIFFERENCES, None),
    ] {
        for (mode, combo, expected) in table {
            let (name, action) = match (action, combo.strip_suffix("#repeat")) {
                (_, Some(name)) => (name, KeyAction::Repeat),
                (Some(action), None) => (*combo, action),
                (None, None) => (*combo, KeyAction::Press),
            };
            let (event, native) = us_key_down(name);
            let actual = pane_bytes(&event, native, mode_state(mode), action);
            if actual.as_deref() != Ok(*expected) {
                failures.push(format!(
                    "{mode} {combo} {action:?}: expected {:?}, got {actual:?}",
                    String::from_utf8_lossy(expected)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Writes Diri's bytes for every combo in a reference matrix
/// (`DIRI_KEY_AUDIT_REF` → `DIRI_KEY_AUDIT_OUT`), for diffing.
#[test]
#[ignore = "audit harness; needs a recorded reference matrix"]
fn dump_key_encoding_audit() {
    let reference: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(std::env::var("DIRI_KEY_AUDIT_REF").unwrap()).unwrap(),
    )
    .unwrap();
    let mut out = serde_json::Map::new();
    for (mode, table) in reference.as_object().unwrap() {
        let keyboard = mode_state(mode);
        let mut rows = serde_json::Map::new();
        for (combo, _) in table.as_object().unwrap() {
            if combo == "__release" {
                let mut releases = serde_json::Map::new();
                for (combo, _) in table[combo].as_object().unwrap() {
                    let (name, action) = match combo.strip_suffix("#repeat") {
                        Some(name) => (name, KeyAction::Repeat),
                        None => (combo.as_str(), KeyAction::Release),
                    };
                    let (event, native) = us_key_down(name);
                    let value = pane_bytes(&event, native, keyboard, action)
                        .map_or_else(|e| format!("ERR:{e}"), |b| hex(&b));
                    releases.insert(combo.clone(), value.into());
                }
                rows.insert(combo.clone(), releases.into());
                continue;
            }
            let (event, native) = us_key_down(combo);
            let value = pane_bytes(&event, native, keyboard, KeyAction::Press)
                .map_or_else(|e| format!("ERR:{e}"), |b| hex(&b));
            rows.insert(combo.clone(), value.into());
        }
        out.insert(mode.clone(), rows.into());
    }
    std::fs::write(
        std::env::var("DIRI_KEY_AUDIT_OUT").unwrap(),
        serde_json::to_string_pretty(&out).unwrap(),
    )
    .unwrap();
}
