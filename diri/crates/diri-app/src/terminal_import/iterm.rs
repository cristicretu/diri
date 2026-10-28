//! iTerm2: the default profile in `com.googlecode.iterm2.plist` (or in the
//! custom preferences folder iTerm2 can be pointed at).
//!
//! iTerm2 writes every key of a profile, set or not, so there is no "only
//! what you changed" here: the default profile is the setup. For the same
//! reason its many other keys are not listed as not imported.

use std::path::PathBuf;

use plist::{Dictionary, Value};

use super::{Colors, LineHeight, Roots, Setup, SkipReason, ThemeChoice, read_bytes};
use crate::i18n::t;
use crate::store::CursorShapePref;

const PLIST: &str = "com.googlecode.iterm2.plist";

pub(super) fn plist_file(roots: &Roots) -> Option<PathBuf> {
    let path = roots.home.join("Library/Preferences").join(PLIST);
    path.is_file().then_some(path)
}

pub(super) fn read(roots: &Roots) -> Option<Setup> {
    let mut file = plist_file(roots)?;
    let mut prefs = load(&file)?;
    // "Load preferences from a custom folder": the real settings live there.
    let custom = prefs
        .get("LoadPrefsFromCustomFolder")
        .and_then(Value::as_boolean)
        .unwrap_or(false);
    if custom
        && let Some(folder) = prefs.get("PrefsCustomFolder").and_then(Value::as_string)
    {
        let folder = super::resolve_path(folder, &roots.home, &roots.home);
        let candidate = folder.join(PLIST);
        if let Some(loaded) = load(&candidate) {
            file = candidate;
            prefs = loaded;
        }
    }
    let mut setup = Setup {
        files: vec![file],
        ..Setup::default()
    };
    if let Some(copy) = prefs.get("CopySelection").and_then(Value::as_boolean) {
        setup.copy_on_select = Some(copy);
    }
    let Some(profile) = default_profile(&prefs) else {
        return Some(setup);
    };
    read_profile(profile, &mut setup);
    Some(setup)
}

fn load(path: &std::path::Path) -> Option<Dictionary> {
    let bytes = read_bytes(path)?;
    plist::Value::from_reader(std::io::Cursor::new(bytes))
        .ok()?
        .into_dictionary()
}

fn default_profile(prefs: &Dictionary) -> Option<&Dictionary> {
    let profiles = prefs.get("New Bookmarks")?.as_array()?;
    let guid = prefs.get("Default Bookmark Guid").and_then(Value::as_string);
    let by_guid = guid.and_then(|guid| {
        profiles.iter().filter_map(Value::as_dictionary).find(|profile| {
            profile.get("Guid").and_then(Value::as_string) == Some(guid)
        })
    });
    by_guid.or_else(|| profiles.first().and_then(Value::as_dictionary))
}

fn read_profile(profile: &Dictionary, setup: &mut Setup) {
    if let Some(font) = profile.get("Normal Font").and_then(Value::as_string)
        && let Some((name, size)) = font.trim().rsplit_once(' ')
    {
        setup.font_family = Some(name.to_owned());
        setup.font_size = size.parse().ok();
    }
    if let Some(spacing) = profile.get("Vertical Spacing").and_then(real) {
        setup.line_height = Some(LineHeight::Scale(spacing));
    }
    setup.cursor_shape = match profile.get("Cursor Type").and_then(integer) {
        Some(0) => Some(CursorShapePref::Underline),
        Some(1) => Some(CursorShapePref::Bar),
        Some(2) => Some(CursorShapePref::Block),
        _ => None,
    };
    setup.cursor_blink = profile
        .get("Blinking Cursor")
        .and_then(Value::as_boolean)
        .map(super::blink_from_bool);
    // 0 is Normal (types characters); 1 Meta and 2 Esc+ both make it Meta.
    // Every profile stores these keys, so all-Normal is iTerm2's default,
    // not a choice, and is left alone.
    let sends = |key: &str| profile.get(key).and_then(integer).unwrap_or(0);
    let (left, right) = (sends("Option Key Sends") != 0, sends("Right Option Key Sends") != 0);
    if left || right {
        setup.option_as_meta = Some(super::option_from_sides(left, right));
    }
    if profile.contains_key("Side Margins") || profile.contains_key("Top/Bottom Margins") {
        setup.skip(t("terminal_import.setting.padding"), SkipReason::Padding);
    }
    if profile.contains_key("Scrollback Lines") || profile.contains_key("Unlimited Scrollback") {
        setup.skip(t("terminal_import.setting.scrollback"), SkipReason::Scrollback);
    }
    if profile
        .get("Keyboard Map")
        .and_then(Value::as_dictionary)
        .is_some_and(|map| !map.is_empty())
    {
        setup.skip(t("terminal_import.setting.key_bindings"), SkipReason::KeyBindings);
    }

    // Profiles with separate light and dark colors keep them under suffixed
    // keys; Diri takes the dark set, its own default.
    let separate = profile
        .get("Use Separate Colors for Light and Dark Mode")
        .and_then(Value::as_boolean)
        .unwrap_or(false);
    let color = |key: &str| {
        let suffixed = format!("{key} (Dark)");
        let entry = if separate {
            profile.get(&suffixed).or_else(|| profile.get(key))
        } else {
            profile.get(key).or_else(|| profile.get(&suffixed))
        };
        entry.and_then(Value::as_dictionary).and_then(component_color)
    };
    let mut colors = Colors {
        background: color("Background Color"),
        foreground: color("Foreground Color"),
        cursor: color("Cursor Color"),
        cursor_text: color("Cursor Text Color"),
        selection: color("Selection Color"),
        ..Colors::default()
    };
    for (index, slot) in colors.ansi.iter_mut().enumerate() {
        *slot = color(&format!("Ansi {index} Color"));
    }
    if !colors.is_empty() {
        let name = profile
            .get("Name")
            .and_then(Value::as_string)
            .filter(|name| !name.is_empty() && *name != "Default")
            .map_or_else(|| "iTerm2".to_owned(), |name| format!("iTerm2 {name}"));
        let spec = colors.into_spec(&name);
        setup.theme = Some(ThemeChoice { name, spec });
    }
}

fn real(value: &Value) -> Option<f32> {
    value
        .as_real()
        .map(|value| value as f32)
        .or_else(|| value.as_signed_integer().map(|value| value as f32))
        .filter(|value| value.is_finite())
}

fn integer(value: &Value) -> Option<i64> {
    value
        .as_signed_integer()
        .or_else(|| value.as_real().map(|value| value as i64))
}

fn component_color(entry: &Dictionary) -> Option<gpui::Rgba> {
    let component = |name: &str| {
        entry
            .get(name)
            .and_then(real)
            .map(|value| value.clamp(0.0, 1.0))
    };
    Some(gpui::Rgba {
        r: component("Red Component")?,
        g: component("Green Component")?,
        b: component("Blue Component")?,
        a: 1.0,
    })
}
