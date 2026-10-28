//! Alacritty: `alacritty.toml`, with `import`ed files read first so the main
//! file wins. The legacy YAML format is detected and named, not parsed.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

use super::{
    Colors, INCLUDE_DEPTH, LineHeight, Roots, Setup, SkipReason, ThemeChoice, parse_color,
    read_text, resolve_path,
};
use crate::i18n::t;
use crate::store::TerminalCursorShape;

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

fn candidates(roots: &Roots, extension: &str) -> Vec<PathBuf> {
    vec![
        roots.xdg_config.join(format!("alacritty/alacritty.{extension}")),
        roots.home.join(format!(".config/alacritty/alacritty.{extension}")),
        roots.home.join(format!(".alacritty.{extension}")),
    ]
}

pub(super) fn read(roots: &Roots) -> Option<Setup> {
    let toml = candidates(roots, "toml").into_iter().find(|path| path.is_file());
    let Some(file) = toml else {
        let yaml = candidates(roots, "yml")
            .into_iter()
            .chain(candidates(roots, "yaml"))
            .find(|path| path.is_file())?;
        let mut setup = Setup {
            files: vec![yaml],
            ..Setup::default()
        };
        setup.skip("alacritty.yml", SkipReason::LegacyFormat);
        return Some(setup);
    };
    let mut setup = Setup::default();
    let mut colors = Colors::default();
    read_file(&file, roots, &mut setup, &mut colors, 0);
    if !colors.is_empty() {
        match colors.into_spec("Alacritty") {
            Some(spec) => {
                setup.theme = Some(ThemeChoice {
                    name: "Alacritty".to_owned(),
                    spec: Some(spec),
                });
            }
            None => setup.skip(t("terminal_import.setting.theme"), SkipReason::Unreadable),
        }
    }
    Some(setup)
}

fn get<'a>(item: &'a Item, path: &[&str]) -> Option<&'a Item> {
    path.iter()
        .try_fold(item, |item, key| item.as_table_like()?.get(key))
}

fn number(item: Option<&Item>) -> Option<f32> {
    let item = item?;
    item.as_float()
        .map(|value| value as f32)
        .or_else(|| item.as_integer().map(|value| value as f32))
}

fn read_file(path: &Path, roots: &Roots, setup: &mut Setup, colors: &mut Colors, depth: usize) {
    if depth > INCLUDE_DEPTH || setup.files.iter().any(|file| file == path) {
        return;
    }
    let Some(text) = read_text(path) else {
        return;
    };
    setup.files.push(path.to_path_buf());
    let Ok(document) = text.parse::<DocumentMut>() else {
        setup.skip("alacritty.toml", SkipReason::Unreadable);
        return;
    };
    let root = document.as_item();
    let base = path.parent().unwrap_or(Path::new("/"));
    // Imports first: the importing file overrides them.
    let imports = get(root, &["general", "import"]).or_else(|| get(root, &["import"]));
    if let Some(imports) = imports.and_then(Item::as_array) {
        for import in imports.iter().filter_map(|value| value.as_str()) {
            let import = resolve_path(import, base, &roots.home);
            read_file(&import, roots, setup, colors, depth + 1);
        }
    }
    let Some(table) = root.as_table_like() else {
        return;
    };
    for (key, item) in table.iter() {
        match key {
            "import" => {}
            "general" => {
                for (sub, _) in item.as_table_like().into_iter().flat_map(|t| t.iter()) {
                    if sub != "import" {
                        setup.unknown(&format!("general.{sub}"));
                    }
                }
            }
            "font" => read_font(item, setup),
            "colors" => read_colors(item, colors, setup),
            "cursor" => read_cursor(item, setup),
            "window" => {
                for (sub, _) in item.as_table_like().into_iter().flat_map(|t| t.iter()) {
                    match sub {
                        "padding" | "dynamic_padding" => {
                            setup.skip(t("terminal_import.setting.padding"), SkipReason::Padding);
                        }
                        "option_as_alt" => setup.skip(
                            t("terminal_import.setting.option_as_alt"),
                            SkipReason::OptionKey,
                        ),
                        other => setup.unknown(&format!("window.{other}")),
                    }
                }
            }
            "scrolling" => {
                setup.skip(t("terminal_import.setting.scrollback"), SkipReason::Scrollback);
            }
            "selection" => {
                if let Some(save) = get(item, &["save_to_clipboard"]).and_then(Item::as_bool) {
                    setup.copy_on_select = Some(save);
                }
            }
            "keyboard" => {
                setup.skip(t("terminal_import.setting.key_bindings"), SkipReason::KeyBindings);
            }
            other => setup.unknown(other),
        }
    }
}

fn read_font(item: &Item, setup: &mut Setup) {
    if let Some(size) = number(get(item, &["size"])) {
        setup.font_size = Some(size);
    }
    if let Some(family) = get(item, &["normal", "family"]).and_then(Item::as_str) {
        setup.font_family = Some(family.to_owned());
    }
    if let Some(offset) = number(get(item, &["offset", "y"])) {
        setup.line_height = Some(LineHeight::Points(offset));
    }
}

fn read_colors(item: &Item, colors: &mut Colors, setup: &mut Setup) {
    let mut color = |path: &[&str]| -> Option<gpui::Rgba> {
        let value = get(item, path)?.as_str()?;
        let parsed = parse_color(value);
        if parsed.is_none() && !value.starts_with("Cell") {
            setup.skip(format!("colors.{}", path.join(".")), SkipReason::Unreadable);
        }
        parsed
    };
    let mut read = Colors {
        background: color(&["primary", "background"]),
        foreground: color(&["primary", "foreground"]),
        cursor: color(&["cursor", "cursor"]),
        cursor_text: color(&["cursor", "text"]),
        selection: color(&["selection", "background"]),
        ..Colors::default()
    };
    for (index, name) in ANSI_NAMES.iter().enumerate() {
        read.ansi[index] = color(&["normal", name]);
        read.ansi[index + 8] = color(&["bright", name]);
    }
    colors.overlay(&read);
}

fn read_cursor(item: &Item, setup: &mut Setup) {
    let style = get(item, &["style"]);
    let shape = style
        .and_then(Item::as_str)
        .or_else(|| style.and_then(|style| get(style, &["shape"])).and_then(Item::as_str));
    if let Some(shape) = shape {
        setup.cursor_shape = match shape.to_ascii_lowercase().as_str() {
            "beam" => Some(TerminalCursorShape::Bar),
            "underline" => Some(TerminalCursorShape::Underline),
            "block" => Some(TerminalCursorShape::Block),
            _ => setup.cursor_shape,
        };
    }
    if let Some(blinking) = style
        .and_then(|style| get(style, &["blinking"]))
        .and_then(Item::as_str)
    {
        setup.cursor_blink = match blinking.to_ascii_lowercase().as_str() {
            "on" | "always" => Some(true),
            "off" | "never" => Some(false),
            _ => setup.cursor_blink,
        };
    }
}
