//! Bring a terminal setup over: font, size, line height, colors, cursor,
//! copy-on-select and Option-as-Meta from the terminal someone used before
//! Diri.
//!
//! Each supported terminal has a reader that turns its configuration into a
//! [`Setup`]: only what the person set, never the source's own defaults.
//! [`plan`] compares that with Diri's preferences and lists exactly what would
//! change, plus everything read but not brought over and why. Applying is one
//! preference write (and at most one theme file), and keeps what it replaced
//! so it can be undone.
//!
//! The source files are only ever read. Nothing is executed (WezTerm's Lua
//! config is scanned for plain literal assignments), and of a setting Diri
//! does not import only its name is kept: values of unknown keys, which may
//! be passwords or tokens, never leave the reader.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use diri_term::theme_file::{self, ThemeSpec};
use diri_term::themes;
use gpui::Rgba;

use crate::i18n::{t, tf};
use crate::store::{CursorBlinkPref, CursorShapePref, OptionAsMeta, Prefs};

mod alacritty;
mod ghostty;
mod iterm;
mod kitty;
pub(crate) mod sheet;
mod terminal_app;
mod wezterm;

#[cfg(test)]
mod tests;

/// Largest config file read. Real ones are a few kilobytes; the cap keeps a
/// corrupt or hostile file from being slurped.
const FILE_CAP: u64 = 1 << 20;
/// Nested `include`s followed before giving up (a cycle, or a mistake).
const INCLUDE_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Source {
    Ghostty,
    ITerm,
    WezTerm,
    Kitty,
    Alacritty,
    TerminalApp,
}

impl Source {
    pub(crate) const ALL: [Self; 6] = [
        Self::Ghostty,
        Self::ITerm,
        Self::WezTerm,
        Self::Kitty,
        Self::Alacritty,
        Self::TerminalApp,
    ];

    /// The product name, as each spells it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Ghostty => "Ghostty",
            Self::ITerm => "iTerm2",
            Self::WezTerm => "WezTerm",
            Self::Kitty => "kitty",
            Self::Alacritty => "Alacritty",
            Self::TerminalApp => "Terminal",
        }
    }
}

/// Where each terminal keeps its configuration for one user.
#[derive(Clone, Debug)]
pub(crate) struct Roots {
    pub home: PathBuf,
    pub xdg_config: PathBuf,
    pub kitty_config_dir: Option<PathBuf>,
    pub wezterm_config_file: Option<PathBuf>,
    /// Folders holding Ghostty's bundled themes, searched after the user's.
    pub ghostty_resources: Vec<PathBuf>,
}

impl Roots {
    pub(crate) fn in_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            xdg_config: home.join(".config"),
            kitty_config_dir: None,
            wezterm_config_file: None,
            ghostty_resources: Vec::new(),
        }
    }

    pub(crate) fn current_user() -> Self {
        let home = diri_platform::home_dir().unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let absolute = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let mut roots = Self::in_home(&home);
        if let Some(xdg) = absolute("XDG_CONFIG_HOME") {
            roots.xdg_config = xdg;
        }
        roots.kitty_config_dir = absolute("KITTY_CONFIG_DIRECTORY");
        roots.wezterm_config_file = absolute("WEZTERM_CONFIG_FILE");
        roots.ghostty_resources = [
            absolute("GHOSTTY_RESOURCES_DIR"),
            Some(PathBuf::from("/Applications/Ghostty.app/Contents/Resources/ghostty")),
            Some(home.join("Applications/Ghostty.app/Contents/Resources/ghostty")),
            Some(PathBuf::from("/usr/share/ghostty")),
        ]
        .into_iter()
        .flatten()
        .map(|dir| dir.join("themes"))
        .collect();
        roots
    }
}

/// A theme as the source names it, with its colors when they could be read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThemeChoice {
    pub name: String,
    pub spec: Option<ThemeSpec>,
}

/// How a source spells a line height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum LineHeight {
    /// A multiple of the font's own line height (1.2 = 20% taller).
    Scale(f32),
    /// Extra points per row.
    Points(f32),
}

/// What one terminal's configuration sets, and nothing it leaves at its own
/// defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Setup {
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub line_height: Option<LineHeight>,
    pub theme: Option<ThemeChoice>,
    pub cursor_shape: Option<CursorShapePref>,
    pub cursor_blink: Option<CursorBlinkPref>,
    pub copy_on_select: Option<bool>,
    pub option_as_meta: Option<OptionAsMeta>,
    /// Read, but Diri has no matching setting: (setting, why).
    pub skipped: Vec<Skipped>,
    /// Names of keys the reader does not know. Names only, never values.
    pub unknown: Vec<String>,
    /// Every file read, for "Read from …".
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Diri's padding is fixed.
    Padding,
    /// Diri bounds history by size itself.
    Scrollback,
    /// Key bindings are not imported; Diri's own shortcuts are in Settings.
    KeyBindings,
    /// A value Diri could not read.
    Unreadable,
    /// The legacy YAML format; `alacritty migrate` converts it.
    LegacyFormat,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Skipped {
    /// What the setting is, in Diri's words ("Option as Alt", "Padding").
    pub setting: String,
    pub reason: SkipReason,
}

impl Setup {
    fn skip(&mut self, setting: impl Into<String>, reason: SkipReason) {
        let setting = setting.into();
        if !self.skipped.iter().any(|skip| skip.setting == setting) {
            self.skipped.push(Skipped { setting, reason });
        }
    }

    fn unknown(&mut self, key: &str) {
        let key = key.trim();
        // A key is a short identifier; anything else is not echoed at all.
        let plausible = !key.is_empty()
            && key.len() <= 64
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
        if plausible && !self.unknown.iter().any(|known| known == key) {
            self.unknown.push(key.to_owned());
        }
    }

    fn read_anything(&self) -> bool {
        self.font_family.is_some()
            || self.font_size.is_some()
            || self.line_height.is_some()
            || self.theme.is_some()
            || self.cursor_shape.is_some()
            || self.cursor_blink.is_some()
            || self.copy_on_select.is_some()
            || self.option_as_meta.is_some()
            || !self.skipped.is_empty()
            || !self.unknown.is_empty()
    }
}

/// A terminal found on this machine and what its configuration says.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Detected {
    pub source: Source,
    pub setup: Setup,
    /// Newest modification time among the files read: the terminal touched
    /// most recently is the one offered first.
    pub modified: Option<SystemTime>,
}

/// Reads every supported terminal's configuration. Terminals with no config
/// file, or one that sets nothing, are left out.
pub(crate) fn scan(roots: &Roots) -> Vec<Detected> {
    let mut found = Vec::new();
    for source in Source::ALL {
        let setup = match source {
            Source::Ghostty => ghostty::read(roots),
            Source::ITerm => iterm::read(roots),
            Source::WezTerm => wezterm::read(roots),
            Source::Kitty => kitty::read(roots),
            Source::Alacritty => alacritty::read(roots),
            Source::TerminalApp => terminal_app::read(roots),
        };
        let Some(setup) = setup.filter(Setup::read_anything) else {
            continue;
        };
        let modified = setup
            .files
            .iter()
            .filter_map(|file| fs::metadata(file).and_then(|meta| meta.modified()).ok())
            .max();
        found.push(Detected {
            source,
            setup,
            modified,
        });
    }
    found
}

/// The scan of one test fixture home (`tests/fixtures/terminal-setup/<name>`).
#[cfg(test)]
pub(crate) fn fixture_scan(name: &str) -> Vec<Detected> {
    let home = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/terminal-setup")
        .join(name);
    let mut roots = Roots::in_home(&home);
    roots.ghostty_resources.clear();
    scan(&roots)
}

/// Reads a config file as text, refusing anything large or not UTF-8.
pub(super) fn read_text(path: &Path) -> Option<String> {
    let bytes = read_bytes(path)?;
    String::from_utf8(bytes).ok()
}

pub(super) fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return None;
    }
    fs::read(path).ok()
}

/// `~/x` and `$HOME/x` against `home`; relative paths against `base`.
pub(super) fn resolve_path(value: &str, base: &Path, home: &Path) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if let Some(rest) = value.strip_prefix("$HOME/") {
        home.join(rest)
    } else {
        base.join(value)
    }
}

/// The colors a config spells out, collected before they become a theme.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Colors {
    pub background: Option<Rgba>,
    pub foreground: Option<Rgba>,
    pub cursor: Option<Rgba>,
    pub cursor_text: Option<Rgba>,
    pub selection: Option<Rgba>,
    pub ansi: [Option<Rgba>; 16],
}

impl Colors {
    pub(super) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Later values win, as in the configs themselves.
    pub(super) fn overlay(&mut self, other: &Self) {
        let pick = |slot: &mut Option<Rgba>, value: Option<Rgba>| {
            if value.is_some() {
                *slot = value;
            }
        };
        pick(&mut self.background, other.background);
        pick(&mut self.foreground, other.foreground);
        pick(&mut self.cursor, other.cursor);
        pick(&mut self.cursor_text, other.cursor_text);
        pick(&mut self.selection, other.selection);
        for (slot, value) in self.ansi.iter_mut().zip(other.ansi) {
            pick(slot, value);
        }
    }

    /// A theme needs its paper and ink; palette slots left out keep xterm's
    /// colors, as every theme file reader does.
    pub(super) fn into_spec(self, name: &str) -> Option<ThemeSpec> {
        let defaults = theme_file::xterm_ansi();
        Some(ThemeSpec {
            name: name.trim().to_owned(),
            appearance: None,
            background: self.background?,
            foreground: self.foreground?,
            cursor: self.cursor,
            cursor_text: self.cursor_text,
            selection: self.selection,
            ansi: std::array::from_fn(|index| self.ansi[index].unwrap_or(defaults[index])),
        })
    }
}

/// `0xrrggbb` as well as everything [`theme_file::parse_color`] reads.
pub(super) fn parse_color(value: &str) -> Option<Rgba> {
    let value = value.trim().trim_matches(['"', '\'']);
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    theme_file::parse_color(value)
}

/// Option as Meta from which sides are Meta.
pub(super) const fn option_from_sides(left: bool, right: bool) -> OptionAsMeta {
    match (left, right) {
        (true, true) => OptionAsMeta::Both,
        (true, false) => OptionAsMeta::Left,
        (false, true) => OptionAsMeta::Right,
        (false, false) => OptionAsMeta::Off,
    }
}

/// `current` with one side's Option made Meta (`meta`) or not; a side not
/// set yet types characters, as it does by default in every terminal here.
pub(super) fn option_with_side(
    current: Option<OptionAsMeta>,
    left: bool,
    meta: bool,
) -> OptionAsMeta {
    let (mut left_meta, mut right_meta) = match current {
        Some(OptionAsMeta::Both) => (true, true),
        Some(OptionAsMeta::Left) => (true, false),
        Some(OptionAsMeta::Right) => (false, true),
        _ => (false, false),
    };
    if left {
        left_meta = meta;
    } else {
        right_meta = meta;
    }
    option_from_sides(left_meta, right_meta)
}

/// A blinking cursor on or off, at Diri's normal speed when on.
pub(super) fn blink_from_bool(blinks: bool) -> CursorBlinkPref {
    if blinks {
        CursorBlinkPref::Normal
    } else {
        CursorBlinkPref::Off
    }
}

/// The speed nearest a full on-and-off cycle of `seconds` (Diri's Fast,
/// Normal and Slow cycles are 0.72, 1.2 and 1.8 s); zero or less is off.
pub(super) fn blink_from_cycle(seconds: f32) -> CursorBlinkPref {
    if !seconds.is_finite() || seconds <= 0.0 {
        CursorBlinkPref::Off
    } else if seconds < 0.96 {
        CursorBlinkPref::Fast
    } else if seconds < 1.5 {
        CursorBlinkPref::Normal
    } else {
        CursorBlinkPref::Slow
    }
}

pub(super) fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().trim_matches(['"', '\'']).to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// `"20%"` → a 1.2 scale, `"2"` → two extra points.
pub(super) fn parse_cell_adjust(value: &str) -> Option<LineHeight> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        let percent: f32 = percent.trim().parse().ok()?;
        return percent
            .is_finite()
            .then_some(LineHeight::Scale(1.0 + percent / 100.0));
    }
    let points: f32 = value.strip_suffix("px").unwrap_or(value).trim().parse().ok()?;
    points.is_finite().then_some(LineHeight::Points(points))
}

// ---------------------------------------------------------------------------
// Planning

/// The theme an import would select.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ThemeTarget {
    /// A theme Diri already has (built in, or imported before with these
    /// very colors).
    Existing(&'static str),
    /// New colors, written to the themes folder on apply.
    Import(Box<ThemeSpec>),
}

/// Exactly what applying writes. `None` leaves a preference as it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Apply {
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub line_height: Option<f32>,
    pub theme: Option<ThemeTarget>,
    pub cursor_shape: Option<CursorShapePref>,
    pub cursor_blink: Option<CursorBlinkPref>,
    pub copy_on_select: Option<bool>,
    pub option_as_meta: Option<OptionAsMeta>,
}

impl Apply {
    /// Writes these values into `prefs`, as one preference edit.
    pub(crate) fn write(&self, prefs: &mut Prefs, theme_id: Option<&str>) {
        if let Some(family) = &self.font_family {
            prefs.terminal_font_family.clone_from(family);
        }
        if let Some(size) = self.font_size {
            prefs.terminal_font_size = size;
        }
        if let Some(scale) = self.line_height {
            prefs.terminal_line_height = scale;
        }
        if let Some(id) = theme_id {
            prefs.follow_system_theme = false;
            id.clone_into(&mut prefs.terminal_theme);
        }
        if let Some(shape) = self.cursor_shape {
            prefs.terminal_cursor_shape = shape;
        }
        if let Some(blink) = self.cursor_blink {
            prefs.terminal_cursor_blink = blink;
        }
        if let Some(copy) = self.copy_on_select {
            prefs.terminal_copy_on_select = copy;
        }
        if let Some(option) = self.option_as_meta {
            prefs.terminal_option_as_meta = option;
        }
    }
}

/// One line of "what will change": the setting, Diri's value now, and the
/// value it becomes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    pub setting: &'static str,
    pub from: String,
    pub to: String,
    /// The new theme, for the sheet's swatch.
    pub swatch: Option<[Rgba; 4]>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub source: Source,
    pub changes: Vec<Change>,
    pub apply: Apply,
    /// Read but not brought over, with the reason, in Diri's words.
    pub not_imported: Vec<String>,
    /// Settings already the same in Diri, by name.
    pub unchanged: Vec<&'static str>,
    pub files: Vec<PathBuf>,
}

impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// "font, theme and cursor": the changes as a short phrase.
    pub(crate) fn summary(&self) -> String {
        let names: Vec<String> = self
            .changes
            .iter()
            .map(|change| change.setting.to_lowercase())
            .collect();
        crate::herdr_import::join_list(&names)
    }
}

/// What the planner needs to know about this Mac: which font families are
/// installed (`None` when a family is missing) and the theme catalog.
pub(crate) struct Environment<'a> {
    pub resolve_font: &'a dyn Fn(&str) -> Option<String>,
}

/// The preference values that would change, from `setup` against `current`.
pub(crate) fn plan(source: Source, setup: &Setup, current: &Prefs, env: &Environment) -> Plan {
    let mut apply = Apply::default();
    let mut not_imported = Vec::new();
    let mut unchanged = Vec::new();
    let mut next = current.clone();

    if let Some(family) = &setup.font_family {
        match (env.resolve_font)(family) {
            Some(installed) => apply.font_family = Some(installed),
            None => not_imported.push(tf(
                "terminal_import.skip.font_missing",
                &[("font", family)],
            )),
        }
    }
    apply.font_size = setup.font_size.filter(|size| size.is_finite() && *size > 0.0);
    let size = apply
        .font_size
        .unwrap_or(current.terminal_font_size)
        .clamp(Prefs::MIN_TERMINAL_FONT_SIZE, Prefs::MAX_TERMINAL_FONT_SIZE);
    apply.line_height = setup.line_height.map(|height| match height {
        LineHeight::Scale(scale) => scale,
        // A row is about 1.2 em at scale 1; extra points add to that.
        LineHeight::Points(points) => 1.0 + points / (size * 1.2),
    });
    apply.cursor_shape = setup.cursor_shape;
    apply.cursor_blink = setup.cursor_blink;
    apply.copy_on_select = setup.copy_on_select;
    // Option is a Mac key; elsewhere Alt is always Meta and the setting has
    // no row, so it stays reported rather than silently stored.
    if cfg!(target_os = "macos") {
        apply.option_as_meta = setup.option_as_meta;
    } else if setup.option_as_meta.is_some() {
        not_imported.push(tf(
            "terminal_import.skip.option_key",
            &[("setting", &t("terminal_import.setting.option_as_alt"))],
        ));
    }

    let mut theme_id: Option<&'static str> = None;
    let mut new_theme: Option<ThemeSpec> = None;
    if let Some(choice) = &setup.theme {
        match resolve_theme(choice) {
            Some(ThemeTarget::Existing(id)) => {
                theme_id = Some(id);
                apply.theme = Some(ThemeTarget::Existing(id));
            }
            Some(ThemeTarget::Import(spec)) => {
                theme_id = Some(themes::intern(&imported_id(&spec)));
                new_theme = Some((*spec).clone());
                apply.theme = Some(ThemeTarget::Import(spec));
            }
            None => not_imported.push(tf(
                "terminal_import.skip.theme_missing",
                &[("theme", &choice.name)],
            )),
        }
    }
    apply.write(&mut next, theme_id);
    next.normalize();

    let mut changes = Vec::new();
    let mut compare = |changes: &mut Vec<Change>,
                       setting: &'static str,
                       from: String,
                       to: String,
                       set: bool| {
        if !set {
            return;
        }
        if from == to {
            unchanged.push(setting);
        } else {
            changes.push(Change {
                setting,
                from,
                to,
                swatch: None,
            });
        }
    };
    let family = |name: &str| {
        if name.is_empty() {
            t("terminal_import.value.default_font").to_owned()
        } else {
            name.to_owned()
        }
    };
    compare(
        &mut changes,
        t("terminal_import.setting.font"),
        family(&current.terminal_font_family),
        family(&next.terminal_font_family),
        apply.font_family.is_some(),
    );
    compare(
        &mut changes,
        t("terminal_import.setting.size"),
        format_points(current.terminal_font_size),
        format_points(next.terminal_font_size),
        apply.font_size.is_some(),
    );
    compare(
        &mut changes,
        t("terminal_import.setting.line_height"),
        format!("{:.1}", current.terminal_line_height),
        format!("{:.1}", next.terminal_line_height),
        apply.line_height.is_some(),
    );
    let theme_name = |id: &str| {
        themes::find(id).map_or_else(|| id.to_owned(), |theme| theme.name.to_owned())
    };
    let current_theme = if current.follow_system_theme {
        t("terminal_import.value.follow_system").to_owned()
    } else {
        theme_name(&current.terminal_theme)
    };
    let next_theme = match (&new_theme, theme_id) {
        (Some(spec), _) => spec.name.clone(),
        (None, Some(id)) => theme_name(id),
        (None, None) => current_theme.clone(),
    };
    let theme_changed = theme_id.is_some()
        && (current.follow_system_theme
            || current.terminal_theme != next.terminal_theme
            // Same id, new colors: re-importing an edited theme.
            || new_theme.as_ref().is_some_and(|spec| {
                themes::find(&next.terminal_theme).is_none_or(|existing| {
                    existing.background != spec.background
                        || existing.foreground != spec.foreground
                        || existing.ansi != spec.ansi
                })
            }));
    if theme_id.is_some() {
        if theme_changed {
            let swatch = new_theme
                .as_ref()
                .map(|spec| [spec.background, spec.foreground, spec.ansi[1], spec.ansi[4]])
                .or_else(|| {
                    theme_id.and_then(themes::find).map(|theme| {
                        [
                            theme.background,
                            theme.foreground,
                            theme.ansi[1],
                            theme.ansi[4],
                        ]
                    })
                });
            changes.push(Change {
                setting: t("terminal_import.setting.theme"),
                from: current_theme,
                to: next_theme,
                swatch,
            });
        } else {
            compare(
                &mut changes,
                t("terminal_import.setting.theme"),
                String::new(),
                String::new(),
                true,
            );
        }
    }
    compare(
        &mut changes,
        t("terminal_import.setting.cursor"),
        cursor_label(current.terminal_cursor_shape, current.terminal_cursor_blink),
        cursor_label(next.terminal_cursor_shape, next.terminal_cursor_blink),
        apply.cursor_shape.is_some() || apply.cursor_blink.is_some(),
    );
    let on_off = |value: bool| {
        t(if value {
            "terminal_import.value.on"
        } else {
            "terminal_import.value.off"
        })
        .to_owned()
    };
    compare(
        &mut changes,
        t("terminal_import.setting.copy_on_select"),
        on_off(current.terminal_copy_on_select),
        on_off(next.terminal_copy_on_select),
        apply.copy_on_select.is_some(),
    );
    compare(
        &mut changes,
        t("settings.terminal.option_key"),
        crate::surface_shell::option_as_meta_label(current.terminal_option_as_meta).to_owned(),
        crate::surface_shell::option_as_meta_label(next.terminal_option_as_meta).to_owned(),
        apply.option_as_meta.is_some(),
    );

    // Keep only what actually changes, so applying writes nothing else.
    let changed = |setting: &str| changes.iter().any(|change| change.setting == setting);
    if !changed(t("terminal_import.setting.font")) {
        apply.font_family = None;
    }
    if !changed(t("terminal_import.setting.size")) {
        apply.font_size = None;
    }
    if !changed(t("terminal_import.setting.line_height")) {
        apply.line_height = None;
    }
    if !changed(t("terminal_import.setting.theme")) {
        apply.theme = None;
    }
    if !changed(t("terminal_import.setting.cursor")) {
        apply.cursor_shape = None;
        apply.cursor_blink = None;
    }
    if !changed(t("terminal_import.setting.copy_on_select")) {
        apply.copy_on_select = None;
    }
    if !changed(t("settings.terminal.option_key")) {
        apply.option_as_meta = None;
    }
    // Store the normalized values, so the write is exactly what was shown.
    apply.font_size = apply.font_size.map(|_| next.terminal_font_size);
    apply.line_height = apply.line_height.map(|_| next.terminal_line_height);

    for skip in &setup.skipped {
        let key = match skip.reason {
            SkipReason::Padding => "terminal_import.skip.padding",
            SkipReason::Scrollback => "terminal_import.skip.scrollback",
            SkipReason::KeyBindings => "terminal_import.skip.key_bindings",
            SkipReason::Unreadable => "terminal_import.skip.unreadable",
            SkipReason::LegacyFormat => "terminal_import.skip.legacy_format",
        };
        not_imported.push(tf(key, &[("setting", &skip.setting)]));
    }
    if !setup.unknown.is_empty() {
        not_imported.push(tf(
            "terminal_import.skip.other",
            &[
                ("count", &setup.unknown.len()),
                ("names", &abbreviated(&setup.unknown, 6)),
            ],
        ));
    }

    Plan {
        source,
        changes,
        apply,
        not_imported,
        unchanged,
        files: setup.files.clone(),
    }
}

/// The plans worth offering, most recently configured terminal first.
/// Terminal comes last: it is on every Mac whether or not anyone set it up.
pub(crate) fn plans(found: &[Detected], current: &Prefs, env: &Environment) -> Vec<Plan> {
    let mut ranked: Vec<&Detected> = found.iter().collect();
    ranked.sort_by_key(|detected| {
        (
            detected.source == Source::TerminalApp,
            std::cmp::Reverse(detected.modified),
        )
    });
    ranked
        .into_iter()
        .map(|detected| plan(detected.source, &detected.setup, current, env))
        .filter(|plan| !plan.is_empty())
        .collect()
}

fn abbreviated(names: &[String], shown: usize) -> String {
    let mut text = names.iter().take(shown).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > shown {
        text.push_str(", …");
    }
    text
}

fn format_points(size: f32) -> String {
    if (size - size.round()).abs() < 0.05 {
        format!("{size:.0} pt")
    } else {
        format!("{size:.1} pt")
    }
}

fn cursor_label(shape: CursorShapePref, blink: CursorBlinkPref) -> String {
    match blink {
        CursorBlinkPref::Off => shape.title().to_owned(),
        CursorBlinkPref::Normal => tf(
            "terminal_import.value.cursor_blinking",
            &[("shape", &shape.title())],
        ),
        speed => tf(
            "terminal_import.value.cursor_blinking_speed",
            &[("shape", &shape.title()), ("speed", &speed.title())],
        ),
    }
}

/// The id a theme of this name gets in the themes folder.
pub(crate) fn imported_id(spec: &ThemeSpec) -> String {
    format!("{}{}", themes::IMPORTED_PREFIX, theme_stem(spec))
}

pub(crate) fn theme_stem(spec: &ThemeSpec) -> String {
    match themes::slug(&spec.name) {
        slug if slug.is_empty() => "theme".to_owned(),
        slug => slug,
    }
}

/// A built-in (or already imported) theme with the same colors beats a new
/// file; a name alone (WezTerm's built-in schemes) matches by name.
fn resolve_theme(choice: &ThemeChoice) -> Option<ThemeTarget> {
    let same_colors = |spec: &ThemeSpec, theme: &diri_term::theme::TermTheme| {
        close(spec.background, theme.background)
            && close(spec.foreground, theme.foreground)
            && spec
                .ansi
                .iter()
                .zip(theme.ansi.iter())
                .all(|(a, b)| close(*a, *b))
    };
    match &choice.spec {
        Some(spec) => {
            // Built-in first: an earlier import of the same colors is the
            // second choice.
            let built_in = themes::catalog().iter().find(|theme| same_colors(spec, theme));
            let imported = || {
                themes::imported()
                    .into_iter()
                    .find(|theme| same_colors(spec, theme))
            };
            if let Some(theme) = built_in.or_else(imported) {
                return Some(ThemeTarget::Existing(theme.id));
            }
            Some(ThemeTarget::Import(Box::new(spec.clone())))
        }
        None => {
            let wanted = themes::slug(&choice.name);
            themes::catalog()
                .iter()
                .find(|theme| themes::slug(theme.name) == wanted)
                .map(|theme| ThemeTarget::Existing(theme.id))
        }
    }
}

fn close(a: Rgba, b: Rgba) -> bool {
    let near = |x: f32, y: f32| (x - y).abs() < 1.5 / 255.0;
    near(a.r, b.r) && near(a.g, b.g) && near(a.b, b.b)
}

/// Matches a configured family (or a PostScript face name such as
/// `JetBrainsMono-Regular`) against installed family names.
pub(crate) fn match_font_family<'a>(
    wanted: &str,
    installed: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let wanted = wanted.trim().trim_matches(['"', '\'']);
    if wanted.is_empty() {
        return None;
    }
    let key = |name: &str| {
        name.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    // PostScript names carry the face after a dash; Nerd Font names
    // abbreviate themselves.
    let base = wanted
        .rsplit_once('-')
        .filter(|(_, face)| !face.contains(' '))
        .map_or(wanted, |(family, _)| family);
    let candidates = [key(wanted), key(base), expand_nerd_font(&key(base))];
    let mut fallback = None;
    for name in installed {
        if name == wanted {
            return Some(name.to_owned());
        }
        let name_key = key(name);
        if fallback.is_none() && candidates.contains(&name_key) {
            fallback = Some(name.to_owned());
        }
    }
    fallback
}

fn expand_nerd_font(key: &str) -> String {
    for (short, long) in [
        ("nfm", "nerdfontmono"),
        ("nfp", "nerdfontpropo"),
        ("nf", "nerdfont"),
    ] {
        if let Some(stem) = key.strip_suffix(short) {
            return format!("{stem}{long}");
        }
    }
    key.to_owned()
}

// ---------------------------------------------------------------------------
// Applying and undoing

/// What an applied import replaced, so Undo can put it back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Undo {
    pub source: Source,
    pub previous: PreviousPrefs,
    /// The theme file written, and what was there before (None = nothing).
    pub theme_file: Option<(PathBuf, Option<Vec<u8>>)>,
}

/// The preference values an import can touch, as they were.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreviousPrefs {
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub theme: String,
    pub follow_system_theme: bool,
    pub cursor_shape: CursorShapePref,
    pub cursor_blink: CursorBlinkPref,
    pub copy_on_select: bool,
    pub option_as_meta: OptionAsMeta,
}

impl PreviousPrefs {
    pub(crate) fn of(prefs: &Prefs) -> Self {
        Self {
            font_family: prefs.terminal_font_family.clone(),
            font_size: prefs.terminal_font_size,
            line_height: prefs.terminal_line_height,
            theme: prefs.terminal_theme.clone(),
            follow_system_theme: prefs.follow_system_theme,
            cursor_shape: prefs.terminal_cursor_shape,
            cursor_blink: prefs.terminal_cursor_blink,
            copy_on_select: prefs.terminal_copy_on_select,
            option_as_meta: prefs.terminal_option_as_meta,
        }
    }

    pub(crate) fn restore(&self, prefs: &mut Prefs) {
        prefs.terminal_font_family.clone_from(&self.font_family);
        prefs.terminal_font_size = self.font_size;
        prefs.terminal_line_height = self.line_height;
        prefs.terminal_theme.clone_from(&self.theme);
        prefs.follow_system_theme = self.follow_system_theme;
        prefs.terminal_cursor_shape = self.cursor_shape;
        prefs.terminal_cursor_blink = self.cursor_blink;
        prefs.terminal_copy_on_select = self.copy_on_select;
        prefs.terminal_option_as_meta = self.option_as_meta;
    }
}

#[derive(Debug)]
pub(crate) enum ApplyError {
    Theme,
    Prefs,
}

/// Writes the plan's theme (if any) into `themes_dir`, then hands the
/// preference edit to `save`, which must write all of it or nothing. When
/// `save` fails the theme file is put back, so a failed apply leaves no trace.
pub(crate) fn apply(
    plan: &Plan,
    themes_dir: &Path,
    current: &Prefs,
    save: impl FnOnce(&dyn Fn(&mut Prefs)) -> std::io::Result<()>,
) -> Result<Undo, ApplyError> {
    let previous = PreviousPrefs::of(current);
    let mut theme_file = None;
    let theme_id: Option<String> = match &plan.apply.theme {
        None => None,
        Some(ThemeTarget::Existing(id)) => Some((*id).to_owned()),
        Some(ThemeTarget::Import(spec)) => {
            let path = themes_dir.join(format!(
                "{}.{}",
                theme_stem(spec),
                theme_file::DIRI_THEME_EXTENSION
            ));
            let before = fs::read(&path).ok();
            let theme = crate::imported_themes::import(
                themes_dir,
                &path.file_name().unwrap_or_default().to_string_lossy(),
                theme_file::write_diri(spec).as_bytes(),
            )
            .map_err(|_| ApplyError::Theme)?;
            theme_file = Some((path, before));
            Some(theme.id.to_owned())
        }
    };
    let edit = |prefs: &mut Prefs| plan.apply.write(prefs, theme_id.as_deref());
    if save(&edit).is_err() {
        if let Some((path, before)) = &theme_file {
            restore_theme_file(path, before.as_deref());
        }
        return Err(ApplyError::Prefs);
    }
    Ok(Undo {
        source: plan.source,
        previous,
        theme_file,
    })
}

/// Puts back what `undo` recorded: the preferences in one edit, then the
/// theme file as it was.
pub(crate) fn undo(
    undo: &Undo,
    save: impl FnOnce(&dyn Fn(&mut Prefs)) -> std::io::Result<()>,
) -> std::io::Result<()> {
    save(&|prefs: &mut Prefs| undo.previous.restore(prefs))?;
    if let Some((path, before)) = &undo.theme_file {
        restore_theme_file(path, before.as_deref());
    }
    Ok(())
}

fn restore_theme_file(path: &Path, before: Option<&[u8]>) {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_owned();
    let id = format!("{}{stem}", themes::IMPORTED_PREFIX);
    match before {
        Some(bytes) => {
            if fs::write(path, bytes).is_ok()
                && let Some(dir) = path.parent()
                && let Some(theme) = crate::imported_themes::load(dir)
                    .into_iter()
                    .find(|theme| theme.id == id)
            {
                themes::add_imported(theme);
            }
        }
        None => {
            let _ = fs::remove_file(path);
            themes::remove_imported(&id);
        }
    }
}
