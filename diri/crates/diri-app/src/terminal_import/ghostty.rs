//! Ghostty: `key = value` lines in `~/.config/ghostty/config` (and the macOS
//! Application Support copy), with `config-file` includes and named themes.

use std::path::{Path, PathBuf};

use super::{
    Colors, INCLUDE_DEPTH, Roots, Setup, SkipReason, ThemeChoice, parse_bool, parse_cell_adjust,
    parse_color, read_text, resolve_path,
};
use crate::i18n::t;
use crate::store::CursorShapePref;

pub(super) fn config_files(roots: &Roots) -> Vec<PathBuf> {
    let xdg = roots.xdg_config.join("ghostty");
    let support = roots
        .home
        .join("Library/Application Support/com.mitchellh.ghostty");
    [
        xdg.join("config"),
        xdg.join("config.ghostty"),
        support.join("config"),
        support.join("config.ghostty"),
    ]
    .into_iter()
    .filter(|path| path.is_file())
    .collect()
}

#[derive(Default)]
struct State {
    setup: Setup,
    colors: Colors,
    theme: Option<String>,
    /// The font family list after the last reset; the first entry is the
    /// face Ghostty draws with.
    families: Vec<String>,
}

pub(super) fn read(roots: &Roots) -> Option<Setup> {
    let files = config_files(roots);
    if files.is_empty() {
        return None;
    }
    let mut state = State::default();
    for file in files {
        read_file(&file, roots, &mut state, 0);
    }
    let State {
        mut setup,
        colors,
        theme,
        families,
    } = state;
    setup.font_family = families.into_iter().next();
    setup.theme = match theme {
        Some(name) => {
            let mut base = Colors::default();
            let found = find_theme(&name, roots).and_then(|path| {
                let text = read_text(&path)?;
                setup.files.push(path);
                Some(text)
            });
            match found {
                Some(text) => {
                    let mut ignored = Setup::default();
                    parse_colors(&text, &mut base, &mut ignored);
                    base.overlay(&colors);
                    let label = theme_label(&name);
                    Some(ThemeChoice {
                        spec: base.into_spec(&label),
                        name: label,
                    })
                }
                // A Ghostty name Diri may still know (its catalog carries
                // Ghostty's own themes).
                None if colors.is_empty() => Some(ThemeChoice {
                    name: theme_label(&name),
                    spec: None,
                }),
                None => Some(ThemeChoice {
                    spec: colors.into_spec("Ghostty"),
                    name: "Ghostty".to_owned(),
                }),
            }
        }
        None if !colors.is_empty() => Some(ThemeChoice {
            spec: colors.into_spec("Ghostty"),
            name: "Ghostty".to_owned(),
        }),
        None => None,
    };
    if setup.theme.as_ref().is_some_and(|theme| {
        theme.spec.is_none() && theme.name == "Ghostty"
    }) {
        setup.theme = None;
        setup.skip(t("terminal_import.setting.theme"), SkipReason::Unreadable);
    }
    Some(setup)
}

fn read_file(path: &Path, roots: &Roots, state: &mut State, depth: usize) {
    if depth > INCLUDE_DEPTH || state.setup.files.iter().any(|file| file == path) {
        return;
    }
    let Some(text) = read_text(path) else {
        return;
    };
    state.setup.files.push(path.to_path_buf());
    let base = path.parent().unwrap_or(Path::new("/"));
    // Ghostty loads `config-file`s after the file that names them.
    let mut includes = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = split(line) else {
            continue;
        };
        if key == "config-file" {
            let optional = value.strip_prefix('?').unwrap_or(value);
            if !optional.is_empty() {
                includes.push(resolve_path(optional, base, &roots.home));
            }
            continue;
        }
        apply(key, value, state);
    }
    for include in includes {
        read_file(&include, roots, state, depth + 1);
    }
}

/// One `key = value` line; comments and blank lines are `None`.
fn split(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value);
    Some((key.trim(), value))
}

fn apply(key: &str, value: &str, state: &mut State) {
    let setup = &mut state.setup;
    if color_key(key, value, &mut state.colors, setup) {
        return;
    }
    match key {
        "font-family" => {
            if value.is_empty() {
                state.families.clear();
            } else {
                state.families.push(value.to_owned());
            }
        }
        "font-size" => setup.font_size = value.parse().ok().or(setup.font_size),
        "adjust-cell-height" => {
            setup.line_height = if value.is_empty() {
                None
            } else {
                parse_cell_adjust(value).or(setup.line_height)
            };
        }
        "theme" => {
            state.theme = (!value.is_empty()).then(|| value.to_owned());
        }
        "cursor-style" => {
            setup.cursor_shape = match value {
                "bar" => Some(CursorShapePref::Bar),
                "underline" => Some(CursorShapePref::Underline),
                "block" | "block_hollow" => Some(CursorShapePref::Block),
                _ => setup.cursor_shape,
            };
        }
        "cursor-style-blink" => setup.cursor_blink = parse_bool(value).map(super::blink_from_bool),
        "copy-on-select" => {
            setup.copy_on_select = match value {
                "clipboard" => Some(true),
                other => parse_bool(other).or(setup.copy_on_select),
            };
        }
        "macos-option-as-alt" => {
            setup.skip(t("terminal_import.setting.option_as_alt"), SkipReason::OptionKey);
        }
        "window-padding-x" | "window-padding-y" | "window-padding-balance" => {
            setup.skip(t("terminal_import.setting.padding"), SkipReason::Padding);
        }
        "scrollback-limit" => {
            setup.skip(t("terminal_import.setting.scrollback"), SkipReason::Scrollback);
        }
        "keybind" => {
            setup.skip(t("terminal_import.setting.key_bindings"), SkipReason::KeyBindings);
        }
        other => setup.unknown(other),
    }
}

/// The color keys a Ghostty config and a Ghostty theme file share.
fn color_key(key: &str, value: &str, colors: &mut Colors, setup: &mut Setup) -> bool {
    let slot = match key {
        "background" => &mut colors.background,
        "foreground" => &mut colors.foreground,
        "cursor-color" => &mut colors.cursor,
        "cursor-text" => &mut colors.cursor_text,
        "selection-background" => &mut colors.selection,
        "palette" => {
            let parsed = value.split_once('=').and_then(|(index, color)| {
                Some((index.trim().parse::<usize>().ok()?, parse_color(color)?))
            });
            match parsed {
                Some((index @ 0..16, color)) => colors.ansi[index] = Some(color),
                // The 256-color cube is not themed in Diri.
                Some(_) => {}
                None => setup.skip(t("terminal_import.setting.palette"), SkipReason::Unreadable),
            }
            return true;
        }
        _ => return false,
    };
    match parse_color(value) {
        Some(color) => *slot = Some(color),
        None if value.is_empty() => *slot = None,
        // X11 color names and the like.
        None => setup.skip(key, SkipReason::Unreadable),
    }
    true
}

pub(super) fn parse_colors(text: &str, colors: &mut Colors, setup: &mut Setup) {
    for line in text.lines() {
        if let Some((key, value)) = split(line) {
            color_key(key, value, colors, setup);
        }
    }
}

/// `light:A,dark:B` names a theme per appearance; Diri imports the dark one,
/// as its own default is dark.
fn theme_name(value: &str) -> &str {
    if !value.contains(':') || value.starts_with('/') {
        return value;
    }
    let mut chosen = value;
    for part in value.split(',') {
        if let Some((side, name)) = part.split_once(':') {
            match side.trim() {
                "dark" => return name.trim(),
                "light" => chosen = name.trim(),
                _ => {}
            }
        }
    }
    chosen
}

fn theme_label(value: &str) -> String {
    let name = theme_name(value);
    Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name)
        .to_owned()
}

fn find_theme(value: &str, roots: &Roots) -> Option<PathBuf> {
    let name = theme_name(value);
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    // A bare name only: never walk out of a themes folder.
    if name.contains('/') || name.contains("..") {
        return None;
    }
    std::iter::once(roots.xdg_config.join("ghostty/themes"))
        .chain(std::iter::once(
            roots
                .home
                .join("Library/Application Support/com.mitchellh.ghostty/themes"),
        ))
        .chain(roots.ghostty_resources.iter().cloned())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}
