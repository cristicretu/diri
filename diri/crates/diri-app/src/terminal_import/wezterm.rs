//! WezTerm: its config is a Lua program, which Diri never runs. Only plain
//! top-level assignments of literals are read (`config.font_size = 13`,
//! `font = wezterm.font 'JetBrains Mono'`, `color_scheme = "Dracula"`);
//! anything computed is left alone.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

use super::{
    Colors, LineHeight, Roots, Setup, SkipReason, ThemeChoice, parse_color, read_text,
};
use crate::i18n::t;
use crate::store::CursorShapePref;

pub(super) fn config_file(roots: &Roots) -> Option<PathBuf> {
    roots
        .wezterm_config_file
        .iter()
        .cloned()
        .chain([
            roots.xdg_config.join("wezterm/wezterm.lua"),
            roots.home.join(".config/wezterm/wezterm.lua"),
            roots.home.join(".wezterm.lua"),
        ])
        .find(|path| path.is_file())
}

/// A Lua literal, as far as this reader goes.
#[derive(Debug, PartialEq)]
enum Literal {
    Text(String),
    Number(f32),
    Bool(bool),
    /// The first family of `wezterm.font(...)` or `font_with_fallback`.
    Font(String),
}

pub(super) fn read(roots: &Roots) -> Option<Setup> {
    let file = config_file(roots)?;
    let text = read_text(&file)?;
    let mut setup = Setup {
        files: vec![file.clone()],
        ..Setup::default()
    };
    let mut blink_rate = None;
    for (key, value) in assignments(&text) {
        if key == "cursor_blink_rate" {
            // Milliseconds per phase; 0 never blinks.
            blink_rate = match value {
                Some(Literal::Number(ms)) => Some(ms),
                _ => None,
            };
            continue;
        }
        apply(&key, value, &mut setup, roots, &file);
    }
    if let (Some(ms), Some(blink)) = (blink_rate, setup.cursor_blink)
        && blink != crate::store::CursorBlinkPref::Off
    {
        setup.cursor_blink = Some(super::blink_from_cycle(2.0 * ms / 1000.0));
    }
    Some(setup)
}

/// Top-level `name = literal` assignments: either fields of the config
/// object (`config.x = ...`) or entries of a returned table literal. Lines
/// inside functions, conditionals and loops are skipped, since they may never
/// run.
fn assignments(text: &str) -> Vec<(String, Option<Literal>)> {
    let mut found = Vec::new();
    let mut blocks = 0usize;
    let mut braces = 0usize;
    // Inside `return { ... }` at the top level.
    let mut in_return = false;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let opening_blocks = blocks;
        let opening_braces = braces;
        if opening_blocks == 0 && opening_braces == 0 {
            in_return = line
                .strip_prefix("return")
                .is_some_and(|rest| rest.trim_start().starts_with('{'));
        }
        for word in line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            match word {
                "function" | "if" | "for" | "while" | "repeat" => blocks += 1,
                "end" | "until" => blocks = blocks.saturating_sub(1),
                _ => {}
            }
        }
        for c in line.chars() {
            match c {
                '{' => braces += 1,
                '}' => braces = braces.saturating_sub(1),
                _ => {}
            }
        }
        if opening_blocks > 0 || line.starts_with("local ") {
            continue;
        }
        let Some((target, value)) = line.split_once('=') else {
            continue;
        };
        if value.starts_with('=') || target.ends_with(['~', '<', '>', '=']) {
            continue;
        }
        let target = target.trim();
        let key = match target.rsplit_once('.') {
            // `config.font_size`: a field of the config object.
            Some((object, key)) if opening_braces == 0 && is_identifier(object) => key,
            // `font_size = 13,` directly inside `return {`.
            None if opening_braces == 1 && in_return => target,
            _ => continue,
        };
        if !is_identifier(key) {
            continue;
        }
        let value = value.trim().trim_end_matches(',').trim();
        found.push((key.to_owned(), literal(value)));
    }
    found
}

#[cfg(test)]
pub(super) fn assignment_keys(text: &str) -> Vec<String> {
    assignments(text).into_iter().map(|(key, _)| key).collect()
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !text.starts_with(|c: char| c.is_ascii_digit())
}

/// Drops a `--` comment that is not inside a string.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let bytes = line.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        match (quote, byte) {
            (None, b'"' | b'\'') => quote = Some(byte),
            (Some(open), _) if byte == open => quote = None,
            (None, b'-') if bytes.get(index + 1) == Some(&b'-') => return &line[..index],
            _ => {}
        }
    }
    line
}

fn string_literal(value: &str) -> Option<String> {
    let value = value.trim();
    let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let inner = value.strip_prefix(quote)?;
    let end = inner.find(quote)?;
    // A literal, not an expression that starts with one (`'a' .. b`).
    inner[end + 1..].trim().is_empty().then(|| inner[..end].to_owned())
}

fn literal(value: &str) -> Option<Literal> {
    if let Some(text) = string_literal(value) {
        return Some(Literal::Text(text));
    }
    match value {
        "true" => return Some(Literal::Bool(true)),
        "false" => return Some(Literal::Bool(false)),
        _ => {}
    }
    if let Ok(number) = value.parse::<f32>() {
        return number.is_finite().then_some(Literal::Number(number));
    }
    for call in ["wezterm.font_with_fallback", "wezterm.font"] {
        let Some(arguments) = value.strip_prefix(call) else {
            continue;
        };
        // `wezterm.font 'X'`, `wezterm.font('X', {...})`,
        // `wezterm.font_with_fallback { 'X', 'Y' }`, `({ family = 'X' })`.
        let arguments = arguments
            .trim()
            .trim_start_matches(['(', '{', ' '])
            .trim_start();
        let arguments = arguments
            .strip_prefix("family")
            .map_or(arguments, |rest| rest.trim_start().trim_start_matches('=').trim_start());
        let quote = arguments.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let inner = &arguments[1..];
        let end = inner.find(quote)?;
        return Some(Literal::Font(inner[..end].to_owned()));
    }
    None
}

fn apply(key: &str, value: Option<Literal>, setup: &mut Setup, roots: &Roots, file: &Path) {
    match (key, value) {
        ("font_size", Some(Literal::Number(size))) => setup.font_size = Some(size),
        ("line_height", Some(Literal::Number(scale))) => {
            setup.line_height = Some(LineHeight::Scale(scale));
        }
        ("font", Some(Literal::Font(family))) => setup.font_family = Some(family),
        ("color_scheme", Some(Literal::Text(name))) => {
            let spec = scheme_file(&name, roots, file).and_then(|(path, colors)| {
                setup.files.push(path);
                colors.into_spec(&name)
            });
            setup.theme = Some(ThemeChoice { name, spec });
        }
        ("default_cursor_style", Some(Literal::Text(style))) => {
            let blinking = style.starts_with("Blinking");
            let shape = style.trim_start_matches("Blinking").trim_start_matches("Steady");
            setup.cursor_shape = match shape {
                "Bar" => Some(CursorShapePref::Bar),
                "Underline" => Some(CursorShapePref::Underline),
                "Block" => Some(CursorShapePref::Block),
                _ => None,
            };
            if setup.cursor_shape.is_some() {
                setup.cursor_blink = Some(super::blink_from_bool(blinking));
            }
        }
        (
            side @ ("send_composed_key_when_left_alt_is_pressed"
            | "send_composed_key_when_right_alt_is_pressed"),
            value,
        ) => match value {
            // Not composing is Meta.
            Some(Literal::Bool(composes)) => {
                setup.option_as_meta = Some(super::option_with_side(
                    setup.option_as_meta,
                    side.contains("left"),
                    !composes,
                ));
            }
            _ => {
                setup.option_as_meta = None;
                setup.skip(t("terminal_import.setting.option_as_alt"), SkipReason::Unreadable);
            }
        },
        ("window_padding", _) => {
            setup.skip(t("terminal_import.setting.padding"), SkipReason::Padding);
        }
        ("scrollback_lines", _) => {
            setup.skip(t("terminal_import.setting.scrollback"), SkipReason::Scrollback);
        }
        ("keys" | "key_tables" | "leader" | "mouse_bindings", _) => {
            setup.skip(t("terminal_import.setting.key_bindings"), SkipReason::KeyBindings);
        }
        ("font_size" | "line_height" | "font" | "color_scheme" | "default_cursor_style", _) => {
            // Computed rather than written out: whatever an earlier literal
            // said may no longer hold, so neither is imported.
            match key {
                "font_size" => setup.font_size = None,
                "line_height" => setup.line_height = None,
                "font" => setup.font_family = None,
                "color_scheme" => setup.theme = None,
                _ => {
                    setup.cursor_shape = None;
                    setup.cursor_blink = None;
                }
            }
            setup.skip(key, SkipReason::Unreadable);
        }
        ("colors", _) => setup.skip(t("terminal_import.setting.theme"), SkipReason::Unreadable),
        (other, _) => setup.unknown(other),
    }
}

/// A scheme from WezTerm's `colors` folder beside the config (or under the
/// XDG config), matched by file name or its `[metadata] name`.
fn scheme_file(name: &str, roots: &Roots, config: &Path) -> Option<(PathBuf, Colors)> {
    let dirs = [
        config.parent().map(|dir| dir.join("colors")),
        Some(roots.xdg_config.join("wezterm/colors")),
    ];
    for dir in dirs.into_iter().flatten() {
        let direct = dir.join(format!("{name}.toml"));
        let mut paths = vec![direct];
        if let Ok(entries) = std::fs::read_dir(&dir) {
            paths.extend(entries.flatten().map(|entry| entry.path()).take(512));
        }
        for path in paths {
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            let Some(document) = read_text(&path).and_then(|text| text.parse::<DocumentMut>().ok())
            else {
                continue;
            };
            let root = document.as_item();
            let named = root
                .get("metadata")
                .and_then(|meta| meta.get("name"))
                .and_then(Item::as_str)
                == Some(name)
                || path.file_stem().and_then(|stem| stem.to_str()) == Some(name);
            if named && let Some(colors) = root.get("colors").map(scheme_colors) {
                return Some((path, colors));
            }
        }
    }
    None
}

fn scheme_colors(item: &Item) -> Colors {
    let color = |key: &str| item.get(key).and_then(Item::as_str).and_then(parse_color);
    let mut colors = Colors {
        background: color("background"),
        foreground: color("foreground"),
        cursor: color("cursor_bg"),
        cursor_text: color("cursor_fg"),
        selection: color("selection_bg"),
        ..Colors::default()
    };
    for (offset, key) in [(0, "ansi"), (8, "brights")] {
        if let Some(list) = item.get(key).and_then(Item::as_array) {
            for (index, value) in list.iter().take(8).enumerate() {
                colors.ansi[offset + index] = value.as_str().and_then(parse_color);
            }
        }
    }
    colors
}
