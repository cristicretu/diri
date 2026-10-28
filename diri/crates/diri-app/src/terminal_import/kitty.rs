//! kitty: `key value` lines in `kitty.conf`, with `include` (and the
//! `current-theme.conf` that `kitten themes` writes and includes).

use std::path::{Path, PathBuf};

use super::{
    Colors, INCLUDE_DEPTH, LineHeight, Roots, Setup, SkipReason, ThemeChoice, parse_cell_adjust,
    parse_color, read_text, resolve_path,
};
use crate::i18n::t;
use crate::store::CursorShapePref;

pub(super) fn config_file(roots: &Roots) -> Option<PathBuf> {
    roots
        .kitty_config_dir
        .iter()
        .cloned()
        .chain(std::iter::once(roots.xdg_config.join("kitty")))
        .chain(std::iter::once(roots.home.join(".config/kitty")))
        .map(|dir| dir.join("kitty.conf"))
        .find(|path| path.is_file())
}

#[derive(Default)]
struct State {
    setup: Setup,
    colors: Colors,
    /// `## name: X` from a theme kitten wrote.
    theme_name: Option<String>,
}

pub(super) fn read(roots: &Roots) -> Option<Setup> {
    let file = config_file(roots)?;
    let mut state = State::default();
    read_file(&file, roots, &mut state, 0);
    let State {
        mut setup,
        colors,
        theme_name,
    } = state;
    if !colors.is_empty() {
        let name = theme_name.unwrap_or_else(|| "kitty".to_owned());
        match colors.into_spec(&name) {
            Some(spec) => {
                setup.theme = Some(ThemeChoice {
                    name,
                    spec: Some(spec),
                });
            }
            None => setup.skip(t("terminal_import.setting.theme"), SkipReason::Unreadable),
        }
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
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("## name:") {
            state.theme_name = Some(name.trim().to_owned());
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(char::is_whitespace)
            .map_or((line, ""), |(key, value)| (key, value.trim()));
        match key {
            // Includes run in place, as kitty reads them.
            "include" => read_file(&resolve_path(value, base, &roots.home), roots, state, depth + 1),
            "globinclude" | "envinclude" | "geninclude" => state.setup.unknown(key),
            _ => apply(key, value, state),
        }
    }
}

fn apply(key: &str, value: &str, state: &mut State) {
    let setup = &mut state.setup;
    let colors = &mut state.colors;
    if let Some(index) = key
        .strip_prefix("color")
        .and_then(|index| index.parse::<usize>().ok())
    {
        if index < 16 {
            set_color(&mut colors.ansi[index], key, value, setup);
        }
        return;
    }
    match key {
        "background" => set_color(&mut colors.background, key, value, setup),
        "foreground" => set_color(&mut colors.foreground, key, value, setup),
        "cursor" => set_color(&mut colors.cursor, key, value, setup),
        "cursor_text_color" => set_color(&mut colors.cursor_text, key, value, setup),
        "selection_background" => set_color(&mut colors.selection, key, value, setup),
        "font_family" => {
            // `auto`, or the newer `family="X" style=...` form.
            let family = value
                .strip_prefix("family=")
                .map_or(value, |rest| {
                    rest.trim_start_matches('"')
                        .split('"')
                        .next()
                        .unwrap_or(rest)
                });
            if !family.is_empty() && family != "monospace" && family != "auto" {
                setup.font_family = Some(family.to_owned());
            }
        }
        "font_size" => setup.font_size = value.parse().ok().or(setup.font_size),
        "adjust_line_height" => {
            setup.line_height = line_height(value).or(setup.line_height);
        }
        "modify_font" => {
            if let Some(adjust) = value.strip_prefix("cell_height") {
                setup.line_height = line_height(adjust).or(setup.line_height);
            } else {
                setup.unknown(key);
            }
        }
        "cursor_shape" => {
            setup.cursor_shape = match value {
                "beam" => Some(CursorShapePref::Bar),
                "underline" => Some(CursorShapePref::Underline),
                "block" => Some(CursorShapePref::Block),
                _ => setup.cursor_shape,
            };
        }
        "cursor_blink_interval" => {
            // Seconds per phase; 0 turns blinking off and a negative value
            // is the system rate.
            setup.cursor_blink = value
                .split_whitespace()
                .next()
                .and_then(|interval| interval.parse::<f32>().ok())
                .map(|interval| {
                    if interval < 0.0 {
                        crate::store::CursorBlinkPref::Normal
                    } else {
                        super::blink_from_cycle(2.0 * interval)
                    }
                });
        }
        "copy_on_select" => {
            setup.copy_on_select = match value {
                "no" => Some(false),
                // `yes`/`clipboard`, or a named buffer.
                _ => Some(true),
            };
        }
        "macos_option_as_alt" => {
            setup.skip(t("terminal_import.setting.option_as_alt"), SkipReason::OptionKey);
        }
        "window_padding_width" | "window_margin_width" | "single_window_padding_width" => {
            setup.skip(t("terminal_import.setting.padding"), SkipReason::Padding);
        }
        "scrollback_lines" | "scrollback_pager_history_size" => {
            setup.skip(t("terminal_import.setting.scrollback"), SkipReason::Scrollback);
        }
        "map" | "mouse_map" | "kitty_mod" | "clear_all_shortcuts" => {
            setup.skip(t("terminal_import.setting.key_bindings"), SkipReason::KeyBindings);
        }
        other => setup.unknown(other),
    }
}

/// kitty's percentages are of the font's own height (`110%` = 1.1×), unlike
/// Ghostty's, which add to it; plain numbers are extra pixels in both.
fn line_height(value: &str) -> Option<LineHeight> {
    if let Some(percent) = value.trim().strip_suffix('%') {
        let percent: f32 = percent.trim().parse().ok()?;
        return percent
            .is_finite()
            .then_some(LineHeight::Scale(percent / 100.0));
    }
    parse_cell_adjust(value)
}

fn set_color(slot: &mut Option<gpui::Rgba>, key: &str, value: &str, setup: &mut Setup) {
    match parse_color(value) {
        Some(color) => *slot = Some(color),
        None if value == "none" => *slot = None,
        None => setup.skip(key, SkipReason::Unreadable),
    }
}
