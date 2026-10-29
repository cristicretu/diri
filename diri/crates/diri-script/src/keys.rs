//! Key names as scripts write them (`cmd-shift-d`, `"ctrl-a c"`), checked
//! without a window so a typo is reported where it was written, and by
//! `dirijor script --check`, instead of being bound to something that never
//! fires. The app parses the same text with GPUI when it binds the keys.

const MODIFIERS: &[&str] = &[
    "cmd",
    "ctrl",
    "alt",
    "shift",
    "fn",
    "super",
    "win",
    "secondary",
];

const NAMED: &[&str] = &[
    "space",
    "enter",
    "escape",
    "backspace",
    "tab",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "left",
    "right",
    "up",
    "down",
];

/// Whether `key` (without modifiers) names a key GPUI can fire.
pub fn known_key(key: &str) -> bool {
    key.chars().count() == 1
        || NAMED.contains(&key)
        || key
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
            .is_some_and(|number| (1..=24).contains(&number))
}

/// Checks one keystroke (`cmd-shift-d`) or a sequence (`"ctrl-a c"`).
pub fn check(keys: &str) -> Result<(), String> {
    let strokes: Vec<&str> = keys.split_whitespace().collect();
    if strokes.is_empty() {
        return Err("keys are empty".into());
    }
    for stroke in strokes {
        check_stroke(stroke).map_err(|why| {
            format!("\"{keys}\" is not a key ({why}); write it like cmd-shift-d or \"ctrl-a c\"")
        })?;
    }
    Ok(())
}

fn check_stroke(stroke: &str) -> Result<(), String> {
    // `cmd--` binds the minus key.
    let (body, minus) = match stroke.strip_suffix("--") {
        Some(body) => (body, true),
        None => (stroke, stroke == "-"),
    };
    if minus {
        return body
            .split('-')
            .filter(|part| !part.is_empty())
            .try_for_each(modifier);
    }
    let parts: Vec<&str> = body.split('-').collect();
    let (key, modifiers) = parts.split_last().expect("split yields one part");
    modifiers.iter().copied().try_for_each(modifier)?;
    let key = key.to_ascii_lowercase();
    if key.is_empty() {
        return Err("no key after the modifiers".into());
    }
    if !known_key(&key) {
        return Err(format!("unknown key name \"{key}\""));
    }
    Ok(())
}

fn modifier(part: &str) -> Result<(), String> {
    if MODIFIERS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(part))
    {
        Ok(())
    } else {
        Err(format!("unknown modifier \"{part}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked_like_settings_stores_them() {
        for good in [
            "cmd-shift-d",
            "ctrl-a c",
            "ctrl-alt-left",
            "f12",
            "h",
            "escape",
            "cmd--",
            "-",
            "cmd-/",
            "shift-H",
        ] {
            assert!(check(good).is_ok(), "{good}: {:?}", check(good));
        }
        for bad in ["cmd-shift-jay", "hyper-d", "", "cmd-", "ctrl-a bogus"] {
            assert!(check(bad).is_err(), "{bad}");
        }
        assert!(check("cmd-shift-jay").unwrap_err().contains("jay"));
    }
}
