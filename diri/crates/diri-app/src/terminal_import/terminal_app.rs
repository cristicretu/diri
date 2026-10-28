//! Terminal: the default profile ("Default Window Settings") in
//! `com.apple.Terminal.plist`. Its font and colors are archived `NSFont` and
//! `NSColor` objects, decoded here from the keyed-archive property list
//! without instantiating anything.
//!
//! Terminal is on every Mac, so an untouched install is not a setup: the
//! stock Basic profile is only offered once it has colors of its own.

use std::path::PathBuf;

use plist::{Dictionary, Value};

use super::{Colors, LineHeight, Roots, Setup, SkipReason, ThemeChoice, read_bytes};
use crate::i18n::t;
use crate::store::CursorShapePref;

const ANSI_KEYS: [&str; 8] = [
    "Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "White",
];

pub(super) fn plist_file(roots: &Roots) -> Option<PathBuf> {
    let path = roots
        .home
        .join("Library/Preferences/com.apple.Terminal.plist");
    path.is_file().then_some(path)
}

pub(super) fn read(roots: &Roots) -> Option<Setup> {
    let file = plist_file(roots)?;
    let prefs = Value::from_reader(std::io::Cursor::new(read_bytes(&file)?))
        .ok()?
        .into_dictionary()?;
    let name = prefs
        .get("Default Window Settings")
        .and_then(Value::as_string)
        .unwrap_or("Basic");
    let profile = prefs
        .get("Window Settings")?
        .as_dictionary()?
        .get(name)?
        .as_dictionary()?;
    let colored = profile.contains_key("BackgroundColor") || profile.contains_key("TextColor");
    if name == "Basic" && !colored {
        return None;
    }
    let mut setup = Setup {
        files: vec![file],
        ..Setup::default()
    };
    if let Some((family, size)) = profile
        .get("Font")
        .and_then(Value::as_data)
        .and_then(archived_font)
    {
        setup.font_family = Some(family);
        setup.font_size = Some(size);
    }
    if let Some(spacing) = profile.get("FontHeightSpacing").and_then(real) {
        setup.line_height = Some(LineHeight::Scale(spacing));
    }
    setup.cursor_shape = match profile.get("CursorType").and_then(Value::as_signed_integer) {
        Some(0) => Some(CursorShapePref::Block),
        Some(1) => Some(CursorShapePref::Underline),
        Some(2) => Some(CursorShapePref::Bar),
        _ => None,
    };
    setup.cursor_blink = profile
        .get("CursorBlink")
        .and_then(Value::as_boolean)
        .map(super::blink_from_bool);
    if profile
        .get("useOptionAsMetaKey")
        .and_then(Value::as_boolean)
        .unwrap_or(false)
    {
        setup.option_as_meta = Some(crate::store::OptionAsMeta::Both);
    }
    if profile.contains_key("ScrollbackLines") || profile.contains_key("ShouldLimitScrollback") {
        setup.skip(t("terminal_import.setting.scrollback"), SkipReason::Scrollback);
    }
    if profile.contains_key("keyMapBoundKeys") {
        setup.skip(t("terminal_import.setting.key_bindings"), SkipReason::KeyBindings);
    }
    if colored {
        let color = |key: &str| {
            profile
                .get(key)
                .and_then(Value::as_data)
                .and_then(archived_color)
        };
        let mut colors = Colors {
            background: color("BackgroundColor"),
            foreground: color("TextColor"),
            cursor: color("CursorColor"),
            selection: color("SelectionColor"),
            ..Colors::default()
        };
        for (index, key) in ANSI_KEYS.iter().enumerate() {
            colors.ansi[index] = color(&format!("ANSI{key}Color"));
            colors.ansi[index + 8] = color(&format!("ANSIBright{key}Color"));
        }
        let label = format!("Terminal {name}");
        setup.theme = Some(ThemeChoice {
            spec: colors.into_spec(&label),
            name: label,
        });
    }
    Some(setup)
}

fn real(value: &Value) -> Option<f32> {
    value
        .as_real()
        .map(|value| value as f32)
        .or_else(|| value.as_signed_integer().map(|value| value as f32))
        .filter(|value| value.is_finite())
}

/// The root object of an `NSKeyedArchiver` property list, and its object
/// table for resolving references.
fn archive_root(bytes: &[u8]) -> Option<(Vec<Value>, Dictionary)> {
    let archive = Value::from_reader(std::io::Cursor::new(bytes))
        .ok()?
        .into_dictionary()?;
    let objects = archive.get("$objects")?.as_array()?.clone();
    let root = archive.get("$top")?.as_dictionary()?.get("root")?;
    let root = resolve(&objects, root)?.as_dictionary()?.clone();
    Some((objects, root))
}

fn resolve<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a Value> {
    match value {
        Value::Uid(uid) => objects.get(usize::try_from(uid.get()).ok()?),
        other => Some(other),
    }
}

/// An archived `NSFont`: its PostScript name and point size.
pub(super) fn archived_font(bytes: &[u8]) -> Option<(String, f32)> {
    let (objects, root) = archive_root(bytes)?;
    let name = resolve(&objects, root.get("NSName")?)?.as_string()?.to_owned();
    let size = root.get("NSSize").and_then(real)?;
    Some((name, size))
}

/// An archived `NSColor` in an RGB or gray color space. Catalog and pattern
/// colors have no components to read and come back `None`.
pub(super) fn archived_color(bytes: &[u8]) -> Option<gpui::Rgba> {
    let (objects, root) = archive_root(bytes)?;
    let numbers = |key: &str| -> Option<Vec<f32>> {
        let data = resolve(&objects, root.get(key)?)?;
        let text = match data {
            Value::Data(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Value::String(text) => text.clone(),
            _ => return None,
        };
        let parsed: Vec<f32> = text
            .trim_end_matches('\0')
            .split_whitespace()
            .filter_map(|part| part.parse().ok())
            .collect();
        (!parsed.is_empty()).then_some(parsed)
    };
    let clamp = |value: f32| value.clamp(0.0, 1.0);
    if let Some(rgb) = numbers("NSRGB").or_else(|| numbers("NSComponents"))
        && rgb.len() >= 3
    {
        return Some(gpui::Rgba {
            r: clamp(rgb[0]),
            g: clamp(rgb[1]),
            b: clamp(rgb[2]),
            a: 1.0,
        });
    }
    let white = numbers("NSWhite")?;
    let level = clamp(*white.first()?);
    Some(gpui::Rgba {
        r: level,
        g: level,
        b: level,
        a: 1.0,
    })
}
