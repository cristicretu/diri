//! Terminal themes as files: iTerm2 `.itermcolors`, Ghostty theme files, and
//! diri's own JSON theme file, which is also what an export writes.
//!
//! Every format parses into a [`ThemeSpec`], the colors a theme author
//! publishes. [`ThemeSpec::into_theme`] turns that into a renderer theme the
//! same way for every source: appearance comes from which way the text sits
//! against the paper, and the selection and find overlays are solved by
//! [`tints::derive`] from the author's selection color, so an imported theme
//! gets the same highlight hierarchy and contrast floor as a built-in one.

use std::fmt;

use gpui::Rgba;
use serde::{Deserialize, Serialize};

use crate::contrast::{composite, contrast_ratio, linear};
use crate::theme::{TermTheme, ThemeAppearance, hex};
use crate::tints::{self, Tints};

/// The value of `format` in a diri theme file. Bump it only for a change an
/// older diri could not read; new optional keys do not need it.
pub const DIRI_THEME_FORMAT: u32 = 1;

/// The extension an exported diri theme gets.
pub const DIRI_THEME_EXTENSION: &str = "diritheme";

/// Files larger than this are not themes; refuse them before parsing.
pub const MAX_THEME_FILE_BYTES: usize = 1 << 20;

/// The colors a theme file defines, before diri derives the rest.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeSpec {
    pub name: String,
    /// `None` when the file does not say; [`Self::appearance`] then decides
    /// from the colors.
    pub appearance: Option<ThemeAppearance>,
    pub background: Rgba,
    pub foreground: Rgba,
    pub cursor: Option<Rgba>,
    pub cursor_text: Option<Rgba>,
    /// The author's selection color. diri uses its cast, not its strength:
    /// see [`tints::derive`].
    pub selection: Option<Rgba>,
    pub ansi: [Rgba; 16],
}

/// Which parser a file needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeFormat {
    ITerm,
    Ghostty,
    Diri,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeFileError {
    TooLarge,
    NotText,
    /// The file parsed but is not a theme of its format.
    Invalid(String),
    /// A required color is missing.
    Missing(&'static str),
    /// A newer diri wrote this file.
    UnsupportedVersion(u32),
}

impl fmt::Display for ThemeFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(formatter, "file is too large to be a theme"),
            Self::NotText => write!(formatter, "file is not text"),
            Self::Invalid(why) => write!(formatter, "not a theme file: {why}"),
            Self::Missing(what) => write!(formatter, "theme has no {what} color"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "theme file format {version} is newer than this diri"
                )
            }
        }
    }
}

impl std::error::Error for ThemeFileError {}

/// Which format `bytes` are in. Ghostty files have no marker of their own,
/// so anything that is neither a property list nor JSON is read as one.
#[must_use]
pub fn detect(bytes: &[u8]) -> ThemeFormat {
    let head = &bytes[..bytes.len().min(512)];
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with(b"bplist") || trimmed.starts_with("<?xml") || trimmed.starts_with("<plist")
    {
        ThemeFormat::ITerm
    } else if trimmed.starts_with('{') {
        ThemeFormat::Diri
    } else {
        ThemeFormat::Ghostty
    }
}

/// Parses a theme file of any supported format. `name` is used when the
/// format carries none (iTerm2 and Ghostty name a theme by its file name).
pub fn parse(name: &str, bytes: &[u8]) -> Result<ThemeSpec, ThemeFileError> {
    if bytes.len() > MAX_THEME_FILE_BYTES {
        return Err(ThemeFileError::TooLarge);
    }
    match detect(bytes) {
        ThemeFormat::ITerm => parse_iterm(name, bytes),
        ThemeFormat::Ghostty => parse_ghostty(
            name,
            std::str::from_utf8(bytes).map_err(|_| ThemeFileError::NotText)?,
        ),
        ThemeFormat::Diri => {
            parse_diri(std::str::from_utf8(bytes).map_err(|_| ThemeFileError::NotText)?)
        }
    }
}

/// The display name a file name implies: no directory, no extension.
#[must_use]
pub fn name_from_file_name(file_name: &str) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = [".itermcolors", ".diritheme", ".json", ".conf", ".theme"]
        .iter()
        .find_map(|extension| {
            base.len()
                .checked_sub(extension.len())
                .filter(|&at| base[at..].eq_ignore_ascii_case(extension))
                .map(|at| &base[..at])
        })
        .unwrap_or(base);
    stem.trim().to_owned()
}

/// An iTerm2 color preset: a property list of `"Ansi N Color"`,
/// `"Background Color"` and friends, each a dictionary of float components.
/// XML and binary plists both parse.
pub fn parse_iterm(name: &str, bytes: &[u8]) -> Result<ThemeSpec, ThemeFileError> {
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|error| ThemeFileError::Invalid(error.to_string()))?;
    let dictionary = value
        .as_dictionary()
        .ok_or_else(|| ThemeFileError::Invalid("top level is not a dictionary".into()))?;
    let color = |key: &str| -> Result<Option<Rgba>, ThemeFileError> {
        let Some(entry) = dictionary.get(key) else {
            return Ok(None);
        };
        let entry = entry
            .as_dictionary()
            .ok_or_else(|| ThemeFileError::Invalid(format!("{key} is not a color")))?;
        let component = |name: &str| {
            entry
                .get(name)
                .and_then(|value| {
                    value
                        .as_real()
                        .or_else(|| value.as_signed_integer().map(|v| v as f64))
                })
                .map(|value| (value as f32).clamp(0.0, 1.0))
        };
        match (
            component("Red Component"),
            component("Green Component"),
            component("Blue Component"),
        ) {
            (Some(r), Some(g), Some(b)) => Ok(Some(Rgba { r, g, b, a: 1.0 })),
            _ => Err(ThemeFileError::Invalid(format!("{key} lacks components"))),
        }
    };
    let mut ansi = DEFAULT_ANSI.map(hex);
    let mut any_ansi = false;
    for (index, slot) in ansi.iter_mut().enumerate() {
        if let Some(value) = color(&format!("Ansi {index} Color"))? {
            *slot = value;
            any_ansi = true;
        }
    }
    let background = color("Background Color")?.ok_or(ThemeFileError::Missing("background"))?;
    let foreground = color("Foreground Color")?.ok_or(ThemeFileError::Missing("foreground"))?;
    if !any_ansi {
        return Err(ThemeFileError::Missing("ANSI palette"));
    }
    Ok(ThemeSpec {
        name: name.trim().to_owned(),
        appearance: None,
        background,
        foreground,
        cursor: color("Cursor Color")?,
        cursor_text: color("Cursor Text Color")?,
        selection: color("Selection Color")?,
        ansi,
    })
}

/// A Ghostty theme: `key = value` lines, `palette = N=#rrggbb` for the
/// palette. Keys that are not colors are ignored, as Ghostty theme files
/// may carry any config.
pub fn parse_ghostty(name: &str, text: &str) -> Result<ThemeSpec, ThemeFileError> {
    let mut ansi = DEFAULT_ANSI.map(hex);
    let mut any_ansi = false;
    let (mut background, mut foreground) = (None, None);
    let (mut cursor, mut cursor_text, mut selection) = (None, None, None);
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ThemeFileError::Invalid(format!(
                "line {} is not `key = value`",
                number + 1
            )));
        };
        let (key, value) = (key.trim(), unquote(value.trim()));
        let parsed = || {
            parse_color(value).ok_or_else(|| {
                ThemeFileError::Invalid(format!("line {}: {value:?} is not a color", number + 1))
            })
        };
        match key {
            "palette" => {
                let Some((index, color)) = value.split_once('=') else {
                    return Err(ThemeFileError::Invalid(format!(
                        "line {}: palette needs `N=color`",
                        number + 1
                    )));
                };
                let index = index.trim().parse::<usize>().ok();
                let color = parse_color(color.trim());
                match (index, color) {
                    (Some(index @ 0..16), Some(color)) => {
                        ansi[index] = color;
                        any_ansi = true;
                    }
                    // The 256-color cube and ramp are not themed in diri.
                    (Some(16..=255), Some(_)) => {}
                    _ => {
                        return Err(ThemeFileError::Invalid(format!(
                            "line {}: bad palette entry",
                            number + 1
                        )));
                    }
                }
            }
            "background" => background = Some(parsed()?),
            "foreground" => foreground = Some(parsed()?),
            "cursor-color" => cursor = Some(parsed()?),
            "cursor-text" => cursor_text = Some(parsed()?),
            "selection-background" => selection = Some(parsed()?),
            _ => {}
        }
    }
    let background = background.ok_or(ThemeFileError::Missing("background"))?;
    let foreground = foreground.ok_or(ThemeFileError::Missing("foreground"))?;
    if !any_ansi {
        return Err(ThemeFileError::Missing("ANSI palette"));
    }
    Ok(ThemeSpec {
        name: name.trim().to_owned(),
        appearance: None,
        background,
        foreground,
        cursor,
        cursor_text,
        selection,
        ansi,
    })
}

/// diri's theme file. Colors are `#rrggbb`; only `background`, `foreground`
/// and all sixteen `ansi` entries are required.
///
/// ```json
/// {
///   "format": 1,
///   "name": "Dracula",
///   "appearance": "dark",
///   "background": "#282a36",
///   "foreground": "#f8f8f2",
///   "cursor": "#f8f8f2",
///   "cursor_text": "#282a36",
///   "selection": "#44475a",
///   "ansi": ["#21222c", "…sixteen in all…"]
/// }
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
struct DiriThemeFile {
    format: u32,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    appearance: Option<String>,
    background: String,
    foreground: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selection: Option<String>,
    ansi: Vec<String>,
}

pub fn parse_diri(text: &str) -> Result<ThemeSpec, ThemeFileError> {
    let file: DiriThemeFile = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|error| ThemeFileError::Invalid(error.to_string()))?;
    if file.format > DIRI_THEME_FORMAT {
        return Err(ThemeFileError::UnsupportedVersion(file.format));
    }
    let color = |what: &str, value: &str| {
        parse_color(value)
            .ok_or_else(|| ThemeFileError::Invalid(format!("{what}: {value:?} is not a color")))
    };
    let optional = |what: &str, value: &Option<String>| {
        value.as_deref().map(|value| color(what, value)).transpose()
    };
    if file.ansi.len() != 16 {
        return Err(ThemeFileError::Invalid(format!(
            "ansi has {} colors, not 16",
            file.ansi.len()
        )));
    }
    let mut ansi = [Rgba::default(); 16];
    for (slot, value) in ansi.iter_mut().zip(&file.ansi) {
        *slot = color("ansi", value)?;
    }
    let appearance = match file.appearance.as_deref() {
        None => None,
        Some("dark") => Some(ThemeAppearance::Dark),
        Some("light") => Some(ThemeAppearance::Light),
        Some(other) => {
            return Err(ThemeFileError::Invalid(format!(
                "appearance {other:?} is not \"dark\" or \"light\""
            )));
        }
    };
    let name = file.name.trim();
    if name.is_empty() {
        return Err(ThemeFileError::Invalid("name is empty".into()));
    }
    Ok(ThemeSpec {
        name: name.to_owned(),
        appearance,
        background: color("background", &file.background)?,
        foreground: color("foreground", &file.foreground)?,
        cursor: optional("cursor", &file.cursor)?,
        cursor_text: optional("cursor_text", &file.cursor_text)?,
        selection: optional("selection", &file.selection)?,
        ansi,
    })
}

/// `spec` as a diri theme file, colors exactly as given. Imports are stored
/// this way, so the author's selection color survives a restart.
#[must_use]
pub fn write_diri(spec: &ThemeSpec) -> String {
    let file = DiriThemeFile {
        format: DIRI_THEME_FORMAT,
        name: spec.name.clone(),
        appearance: Some(
            match spec.appearance() {
                ThemeAppearance::Dark => "dark",
                ThemeAppearance::Light => "light",
            }
            .to_owned(),
        ),
        background: hex_string(spec.background),
        foreground: hex_string(spec.foreground),
        cursor: spec.cursor.map(hex_string),
        cursor_text: spec.cursor_text.map(hex_string),
        selection: spec.selection.map(hex_string),
        ansi: spec.ansi.iter().map(|&color| hex_string(color)).collect(),
    };
    let mut text = serde_json::to_string_pretty(&file).expect("a theme file always serializes");
    text.push('\n');
    text
}

/// `theme` as a diri theme file. Translucent colors (the cursor and the
/// selection overlay) are written as they look over the background, which
/// is what re-importing the file reads them as.
#[must_use]
pub fn export_diri(theme: &TermTheme) -> String {
    let opaque = |color: Rgba| composite(color, theme.background);
    write_diri(&ThemeSpec {
        name: theme.name.to_owned(),
        appearance: Some(theme.appearance),
        background: theme.background,
        foreground: theme.foreground,
        cursor: Some(opaque(theme.cursor)),
        cursor_text: Some(opaque(theme.cursor_text)),
        selection: Some(opaque(theme.selection)),
        ansi: theme.ansi,
    })
}

impl ThemeSpec {
    /// Light when the text is darker than the paper. That is the question
    /// contrast correction asks: whether programs that pick colors for dark
    /// terminals need their colors answered for.
    #[must_use]
    pub fn appearance(&self) -> ThemeAppearance {
        self.appearance
            .unwrap_or_else(|| appearance_of(self.background, self.foreground))
    }

    /// The selection color [`tints::derive`] takes its cast from: the
    /// author's, or a neutral step toward the text when there is none.
    #[must_use]
    pub fn selection_seed(&self) -> Rgba {
        self.selection.unwrap_or_else(|| {
            composite(
                Rgba {
                    a: 0.3,
                    ..self.foreground
                },
                self.background,
            )
        })
    }

    #[must_use]
    pub fn tints(&self) -> Tints {
        tints::derive(
            self.background,
            self.foreground,
            self.selection_seed(),
            &self.ansi,
        )
    }

    /// The renderer theme. `id` and `name` are `'static` because themes are
    /// `Copy` and live for the process; see [`crate::themes::intern`].
    #[must_use]
    pub fn into_theme(&self, id: &'static str, name: &'static str) -> TermTheme {
        let tints = self.tints();
        TermTheme {
            id,
            name,
            appearance: self.appearance(),
            background: self.background,
            foreground: self.foreground,
            cursor: self.cursor.unwrap_or(Rgba {
                a: 0.85,
                ..self.foreground
            }),
            cursor_text: self.cursor_text.unwrap_or(self.background),
            selection: tints.selection,
            find_match: tints.find_match,
            find_match_current: tints.find_match_current,
            ansi: self.ansi,
            blend: 0,
        }
    }
}

#[must_use]
pub(crate) fn appearance_of(background: Rgba, foreground: Rgba) -> ThemeAppearance {
    let luminance = |color: Rgba| {
        let [r, g, b] = linear(color);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    if luminance(foreground) < luminance(background) {
        ThemeAppearance::Light
    } else {
        ThemeAppearance::Dark
    }
}

/// Body text contrast, which the catalog uses to leave out unreadable themes.
#[must_use]
pub fn body_contrast(spec: &ThemeSpec) -> f32 {
    contrast_ratio(spec.foreground, spec.background)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

/// `#rrggbb`, `rrggbb`, `#rgb`, or `#rrggbbaa` (alpha ignored: a theme color
/// is opaque).
#[must_use]
pub fn parse_color(value: &str) -> Option<Rgba> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = match digits.len() {
        3 => {
            let short = u32::from_str_radix(digits, 16).ok()?;
            let expand = |nibble: u32| (nibble & 0xf) * 0x11;
            (expand(short >> 8) << 16) | (expand(short >> 4) << 8) | expand(short)
        }
        6 => u32::from_str_radix(digits, 16).ok()?,
        8 => u32::from_str_radix(&digits[..6], 16).ok()?,
        _ => return None,
    };
    Some(hex(value))
}

#[must_use]
pub fn hex_string(color: Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(color.r),
        byte(color.g),
        byte(color.b)
    )
}

/// xterm's sixteen colors, which every reader here fills unset slots with.
#[must_use]
pub fn xterm_ansi() -> [Rgba; 16] {
    DEFAULT_ANSI.map(hex)
}

/// xterm's palette, for the slots a file leaves out.
const DEFAULT_ANSI: [u32; 16] = [
    0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
    0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
];

#[cfg(test)]
mod tests {
    use super::*;

    const DRACULA_ITERM: &[u8] = include_bytes!("../tests/fixtures/themes/Dracula.itermcolors");
    const DRACULA_GHOSTTY: &str = include_str!("../tests/fixtures/themes/Dracula");
    const FLEXOKI_LIGHT_ITERM: &[u8] =
        include_bytes!("../tests/fixtures/themes/Flexoki Light.itermcolors");
    const FLEXOKI_LIGHT_GHOSTTY: &str = include_str!("../tests/fixtures/themes/Flexoki Light");

    fn assert_close(left: Rgba, right: Rgba, what: &str) {
        for (l, r) in [(left.r, right.r), (left.g, right.g), (left.b, right.b)] {
            // Upstream rounds float components to 8 bits for Ghostty.
            assert!(
                (l - r).abs() <= 1.01 / 255.0,
                "{what}: {left:?} vs {right:?}"
            );
        }
    }

    fn assert_same_colors(left: &ThemeSpec, right: &ThemeSpec) {
        assert_close(left.background, right.background, "background");
        assert_close(left.foreground, right.foreground, "foreground");
        assert_close(left.cursor.unwrap(), right.cursor.unwrap(), "cursor");
        assert_close(
            left.selection.unwrap(),
            right.selection.unwrap(),
            "selection",
        );
        for (index, (l, r)) in left.ansi.iter().zip(&right.ansi).enumerate() {
            assert_close(*l, *r, &format!("ansi {index}"));
        }
    }

    #[test]
    fn detects_each_format_from_its_contents() {
        assert_eq!(detect(DRACULA_ITERM), ThemeFormat::ITerm);
        assert_eq!(detect(DRACULA_GHOSTTY.as_bytes()), ThemeFormat::Ghostty);
        assert_eq!(
            detect(export_diri(&TermTheme::DRACULA).as_bytes()),
            ThemeFormat::Diri
        );
    }

    #[test]
    fn real_itermcolors_and_ghostty_files_of_one_theme_agree() {
        for (iterm, ghostty) in [
            (DRACULA_ITERM, DRACULA_GHOSTTY),
            (FLEXOKI_LIGHT_ITERM, FLEXOKI_LIGHT_GHOSTTY),
        ] {
            let iterm = parse("x", iterm).unwrap();
            let ghostty = parse("x", ghostty.as_bytes()).unwrap();
            assert_same_colors(&iterm, &ghostty);
        }
    }

    #[test]
    fn a_real_dracula_file_reproduces_the_curated_palette() {
        let spec = parse("Dracula", DRACULA_GHOSTTY.as_bytes()).unwrap();
        assert_eq!(spec.name, "Dracula");
        assert_eq!(spec.appearance(), ThemeAppearance::Dark);
        assert_eq!(spec.background, TermTheme::DRACULA.background);
        assert_eq!(spec.foreground, TermTheme::DRACULA.foreground);
        assert_eq!(spec.ansi, TermTheme::DRACULA.ansi);
        // Same palette, same seed: the derivation lands on the curated tints.
        let theme = spec.into_theme("dracula-import", "Dracula");
        assert_close(theme.selection, TermTheme::DRACULA.selection, "selection");
        assert_close(theme.find_match, TermTheme::DRACULA.find_match, "find");
        assert_close(
            theme.find_match_current,
            TermTheme::DRACULA.find_match_current,
            "current",
        );
    }

    #[test]
    fn light_files_are_light() {
        let spec = parse("Flexoki Light", FLEXOKI_LIGHT_ITERM).unwrap();
        assert_eq!(spec.appearance(), ThemeAppearance::Light);
    }

    #[test]
    fn export_round_trips_every_curated_theme() {
        for theme in TermTheme::CATALOG {
            let text = export_diri(&theme);
            let spec = parse("ignored", text.as_bytes()).unwrap();
            assert_eq!(spec.name, theme.name);
            assert_eq!(spec.appearance(), theme.appearance, "{}", theme.id);
            assert_close(spec.background, theme.background, theme.id);
            assert_close(spec.foreground, theme.foreground, theme.id);
            for (l, r) in spec.ansi.iter().zip(&theme.ansi) {
                assert_close(*l, *r, theme.id);
            }
            let again = export_diri(&spec.into_theme(theme.id, theme.name));
            let reparsed = parse_diri(&again).unwrap();
            assert_eq!(reparsed.background, spec.background, "{}", theme.id);
            assert_eq!(reparsed.ansi, spec.ansi, "{}", theme.id);
        }
    }

    #[test]
    fn a_stored_import_keeps_the_authors_colors() {
        let spec = parse("Flexoki Light", FLEXOKI_LIGHT_GHOSTTY.as_bytes()).unwrap();
        assert_eq!(
            parse("x", write_diri(&spec).as_bytes()).unwrap(),
            ThemeSpec {
                appearance: Some(ThemeAppearance::Light),
                ..spec
            }
        );
    }

    #[test]
    fn ghostty_tolerates_comments_quotes_and_other_config() {
        let text = "# my theme\n\nfont-size = 13\nbackground = \"#101010\"\nforeground=ffffff\n\
                    palette = 1=#ff0000\npalette = 200=#123456\n";
        let spec = parse_ghostty("Mine", text).unwrap();
        assert_eq!(spec.background, hex(0x101010));
        assert_eq!(spec.foreground, hex(0xffffff));
        assert_eq!(spec.ansi[1], hex(0xff0000));
        // Slots the file leaves out keep xterm's colors.
        assert_eq!(spec.ansi[2], hex(0x00cd00));
        assert_eq!(spec.selection, None);
    }

    #[test]
    fn malformed_files_are_refused_with_a_reason() {
        assert_eq!(
            parse_ghostty("x", "foreground = #ffffff\npalette = 0=#000000\n"),
            Err(ThemeFileError::Missing("background"))
        );
        assert!(matches!(
            parse_ghostty("x", "background = blue\n"),
            Err(ThemeFileError::Invalid(_))
        ));
        assert!(matches!(
            parse_ghostty("x", "just words\n"),
            Err(ThemeFileError::Invalid(_))
        ));
        assert!(matches!(
            parse("x", b"<?xml version=\"1.0\"?><plist><array/></plist>"),
            Err(ThemeFileError::Invalid(_))
        ));
        assert_eq!(
            parse_diri(
                r##"{"format":2,"name":"n","background":"#000","foreground":"#fff","ansi":[]}"##
            ),
            Err(ThemeFileError::UnsupportedVersion(2))
        );
        assert!(matches!(
            parse_diri(
                r##"{"format":1,"name":"n","background":"#000","foreground":"#fff","ansi":["#000"]}"##
            ),
            Err(ThemeFileError::Invalid(_))
        ));
        assert_eq!(
            parse("x", &vec![b' '; MAX_THEME_FILE_BYTES + 1]),
            Err(ThemeFileError::TooLarge)
        );
        assert_eq!(
            parse("x", &[0xff, 0xfe, 0x00]),
            Err(ThemeFileError::NotText)
        );
    }

    #[test]
    fn colors_parse_in_every_spelling_files_use() {
        assert_eq!(parse_color("#abc"), Some(hex(0xaabbcc)));
        assert_eq!(parse_color("aabbcc"), Some(hex(0xaabbcc)));
        assert_eq!(parse_color("#aabbccdd"), Some(hex(0xaabbcc)));
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color("#abcd"), None);
        assert_eq!(hex_string(hex(0x0a0b0c)), "#0a0b0c");
    }

    #[test]
    fn file_names_become_theme_names() {
        assert_eq!(name_from_file_name("/x/Dracula.itermcolors"), "Dracula");
        assert_eq!(name_from_file_name("Mine.DIRITHEME"), "Mine");
        assert_eq!(name_from_file_name("Builtin Dark"), "Builtin Dark");
    }
}
