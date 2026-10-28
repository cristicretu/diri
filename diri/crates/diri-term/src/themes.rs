//! Every terminal theme diri can show: the curated [`TermTheme::CATALOG`],
//! the vendored iTerm2-Color-Schemes collection, and themes the person
//! imported.
//!
//! The collection is one embedded text file, `themes/iterm2-color-schemes.tsv`,
//! read once on first use. Its selection and find overlays are already
//! solved (see [`crate::tints`]), so loading it is parsing hex and nothing
//! else; `vendored_tints_match_the_derivation` keeps those columns honest.
//! `regenerate_vendored_catalog` (ignored) rebuilds the file from an upstream
//! checkout.
//!
//! Theme ids are persisted in prefs, so they never change: curated ids are
//! fixed, a vendored id is its upstream name as a slug, and an imported id is
//! [`IMPORTED_PREFIX`] plus a slug of its file name.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, RwLock};

use gpui::Rgba;

use crate::theme::{TermTheme, ThemeAppearance};
use crate::theme_file::appearance_of;

/// Ids of imported themes start with this, so they can never collide with a
/// catalog theme, today's or a future one.
pub const IMPORTED_PREFIX: &str = "imported-";

const VENDORED: &str = include_str!("../themes/iterm2-color-schemes.tsv");

/// Below this body contrast a theme is not offered: its default text is
/// unreadable before diri draws anything over it.
pub const MINIMUM_BODY_CONTRAST: f32 = 3.0;

struct Catalog {
    themes: Vec<TermTheme>,
    by_id: HashMap<&'static str, usize>,
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    let mut themes = TermTheme::CATALOG.to_vec();
    themes.extend(vendored());
    let by_id = themes
        .iter()
        .enumerate()
        .map(|(index, theme)| (theme.id, index))
        .collect();
    Catalog { themes, by_id }
});

/// Leaked like the strings in [`intern`]: the picker lists themes by
/// reference, and an import is a person's deliberate act, so this grows by a
/// few hundred bytes per distinct import at most.
static IMPORTED: RwLock<Vec<&'static TermTheme>> = RwLock::new(Vec::new());

/// Curated themes first, in their own order, then the collection by name.
#[must_use]
pub fn catalog() -> &'static [TermTheme] {
    &CATALOG.themes
}

/// The imported themes, by name.
#[must_use]
pub fn imported() -> Vec<&'static TermTheme> {
    IMPORTED
        .read()
        .map(|themes| themes.clone())
        .unwrap_or_default()
}

/// Imported themes, then the catalog.
#[must_use]
pub fn all() -> Vec<TermTheme> {
    let mut themes = imported().into_iter().copied().collect::<Vec<_>>();
    themes.extend_from_slice(catalog());
    themes
}

/// Replaces the imported set, as reading the themes folder produces it.
pub fn set_imported(themes: Vec<TermTheme>) {
    let Ok(mut imported) = IMPORTED.write() else {
        return;
    };
    let mut next = themes
        .into_iter()
        .map(|theme| {
            imported
                .iter()
                .copied()
                .find(|existing| **existing == theme)
                .unwrap_or_else(|| Box::leak(Box::new(theme)))
        })
        .collect::<Vec<&'static TermTheme>>();
    next.sort_by_cached_key(|theme| theme.name.to_lowercase());
    next.dedup_by_key(|theme| theme.id);
    *imported = next;
}

/// Adds or replaces one imported theme.
pub fn add_imported(theme: TermTheme) {
    let mut themes = imported()
        .into_iter()
        .copied()
        .filter(|existing| existing.id != theme.id)
        .collect::<Vec<_>>();
    themes.push(theme);
    set_imported(themes);
}

/// Takes one imported theme out of the registry (its file was removed).
pub fn remove_imported(id: &str) {
    if let Ok(mut imported) = IMPORTED.write() {
        imported.retain(|theme| theme.id != id);
    }
}

#[must_use]
pub fn is_imported(id: &str) -> bool {
    id.starts_with(IMPORTED_PREFIX)
}

/// The theme a persisted id names, wherever it comes from.
#[must_use]
pub fn find(id: &str) -> Option<TermTheme> {
    if is_imported(id) {
        let imported = IMPORTED.read().ok()?;
        return imported
            .iter()
            .find(|theme| theme.id == id)
            .map(|theme| **theme);
    }
    CATALOG.by_id.get(id).map(|&index| CATALOG.themes[index])
}

/// A `'static` copy of `text`. Themes are `Copy` and outlive every window, so
/// an imported theme's id and name are leaked; each distinct string once, so
/// re-importing or re-reading the folder does not grow the leak.
#[must_use]
pub fn intern(text: &str) -> &'static str {
    static STRINGS: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut strings = STRINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&existing) = strings.get(text) {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
    strings.insert(leaked);
    leaked
}

/// `"Rosé Pine Moon (v2)"` → `"rose-pine-moon-v2"`.
#[must_use]
pub fn slug(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for character in name.chars() {
        let folded = match character {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            other => other.to_ascii_lowercase(),
        };
        if folded.is_ascii_alphanumeric() {
            slug.push(folded);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Letters and digits only, so "TokyoNight" and "Tokyo Night" are one theme.
fn squashed(name: &str) -> String {
    slug(name).replace('-', "")
}

/// Upstream names a few of the curated themes differently; these are the
/// same palettes and stay out of the collection.
const CURATED_ALIASES: [&str; 3] = ["atomonedark", "rosepine", "kanagawawave"];

fn vendored() -> Vec<TermTheme> {
    let curated = TermTheme::CATALOG
        .iter()
        .flat_map(|theme| [squashed(theme.id), squashed(theme.name)])
        .chain(CURATED_ALIASES.map(str::to_owned))
        .collect::<HashSet<_>>();
    let mut taken = TermTheme::CATALOG
        .iter()
        .map(|theme| theme.id.to_owned())
        .collect::<HashSet<_>>();
    let mut themes = Vec::new();
    for line in VENDORED.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(row) = Row::parse(line) else {
            debug_assert!(false, "bad vendored theme row: {line}");
            continue;
        };
        if curated.contains(&squashed(row.name)) {
            continue;
        }
        let base = slug(row.name);
        let mut id = base.clone();
        let mut suffix = 2;
        while !taken.insert(id.clone()) {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        // Built once per process, so each id is leaked once.
        let id = intern(&id);
        themes.push(row.theme(id));
    }
    themes
}

/// One line of the vendored file: the name, then space-separated hex colors
/// `background foreground cursor cursor_text seed selection find current
/// ansi0..ansi15`, the three tints as `rrggbbaa`.
struct Row {
    name: &'static str,
    colors: [Rgba; 24],
}

impl Row {
    const COLORS: usize = 24;

    fn parse(line: &'static str) -> Option<Self> {
        let (name, colors) = line.split_once('\t')?;
        let mut parsed = [Rgba::default(); Self::COLORS];
        let mut fields = colors.split(' ');
        for slot in &mut parsed {
            *slot = parse_hex(fields.next()?)?;
        }
        fields.next().is_none().then_some(Self {
            name,
            colors: parsed,
        })
    }

    fn theme(&self, id: &'static str) -> TermTheme {
        let c = &self.colors;
        let mut ansi = [Rgba::default(); 16];
        ansi.copy_from_slice(&c[8..24]);
        TermTheme {
            id,
            name: self.name,
            appearance: appearance_of(c[0], c[1]),
            background: c[0],
            foreground: c[1],
            cursor: c[2],
            cursor_text: c[3],
            selection: c[5],
            find_match: c[6],
            find_match_current: c[7],
            ansi,
            blend: 0,
        }
    }
}

fn parse_hex(field: &str) -> Option<Rgba> {
    let value = u32::from_str_radix(field, 16).ok()?;
    let channel = |shift: u32| ((value >> shift) & 0xff) as f32 / 255.0;
    match field.len() {
        6 => Some(Rgba {
            r: channel(16),
            g: channel(8),
            b: channel(0),
            a: 1.0,
        }),
        8 => Some(Rgba {
            r: channel(24),
            g: channel(16),
            b: channel(8),
            a: channel(0),
        }),
        _ => None,
    }
}

/// Counts by appearance, for the picker's group headers and tests.
#[must_use]
pub fn count(appearance: ThemeAppearance) -> usize {
    catalog()
        .iter()
        .filter(|theme| theme.appearance == appearance)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contrast::{composite, contrast_ratio};
    use crate::theme_file::{ThemeSpec, hex_string};

    fn rows() -> impl Iterator<Item = Row> {
        VENDORED
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| Row::parse(line).expect(line))
    }

    fn spec(row: &Row) -> ThemeSpec {
        let mut ansi = [Rgba::default(); 16];
        ansi.copy_from_slice(&row.colors[8..24]);
        ThemeSpec {
            name: row.name.to_owned(),
            appearance: None,
            background: row.colors[0],
            foreground: row.colors[1],
            cursor: Some(row.colors[2]),
            cursor_text: Some(row.colors[3]),
            selection: Some(row.colors[4]),
            ansi,
        }
    }

    #[test]
    fn the_catalog_holds_hundreds_of_themes_after_the_curated_ones() {
        let catalog = catalog();
        assert!(catalog.len() >= 500, "{}", catalog.len());
        for (index, theme) in TermTheme::CATALOG.iter().enumerate() {
            assert_eq!(catalog[index], *theme, "curated themes keep their place");
        }
        let ids = catalog.iter().map(|theme| theme.id).collect::<HashSet<_>>();
        assert_eq!(ids.len(), catalog.len(), "ids are unique");
        assert!(count(ThemeAppearance::Light) >= 100);
        assert!(count(ThemeAppearance::Dark) >= 300);
    }

    #[test]
    fn curated_ids_resolve_to_the_curated_themes() {
        for theme in TermTheme::CATALOG {
            assert_eq!(find(theme.id), Some(theme));
        }
        // The collection's own copies of curated palettes stay out.
        for name in [
            "Dracula",
            "Nord",
            "TokyoNight",
            "Catppuccin Mocha",
            "Gruvbox Dark",
        ] {
            assert!(
                !catalog()[TermTheme::CATALOG.len()..]
                    .iter()
                    .any(|theme| squashed(theme.name) == squashed(name)),
                "{name}"
            );
        }
        assert_eq!(find("no-such-theme"), None);
    }

    #[test]
    fn vendored_ids_are_slugs_of_their_names() {
        let theme = find("3024-night").expect("upstream's first dark theme");
        assert_eq!(theme.name, "3024 Night");
        assert_eq!(theme.appearance, ThemeAppearance::Dark);
        assert_eq!(find("3024-day").unwrap().appearance, ThemeAppearance::Light);
        assert_eq!(slug("Rosé Pine Moon (v2)"), "rose-pine-moon-v2");
        assert_eq!(slug("Jack O' Lantern"), "jack-o-lantern");
    }

    #[test]
    fn every_vendored_theme_is_readable() {
        for theme in &catalog()[TermTheme::CATALOG.len()..] {
            let own = contrast_ratio(theme.foreground, theme.background);
            assert!(own >= MINIMUM_BODY_CONTRAST, "{}: {own}", theme.name);
        }
    }

    /// The same guarantee `tints` gives the curated themes: default text
    /// stays at 4.5:1 over every overlay, or near the theme's own contrast
    /// when that is lower, and every overlay is a translucent color.
    #[test]
    fn vendored_tints_keep_default_text_readable() {
        let mut below = Vec::new();
        for theme in &catalog()[TermTheme::CATALOG.len()..] {
            let own = contrast_ratio(theme.foreground, theme.background);
            let floor = own.min(4.5) * 0.7;
            for tint in [theme.selection, theme.find_match, theme.find_match_current] {
                let over = contrast_ratio(theme.foreground, composite(tint, theme.background));
                if over < floor {
                    below.push(theme.name);
                }
                assert!((0.19..=1.0).contains(&tint.a), "{}: {}", theme.name, tint.a);
            }
        }
        below.dedup();
        // Red on yellow at 4.2:1: the minimum steps that keep its tints
        // visible at all outrank the last few tenths of contrast, as
        // `tints::derive` orders its priorities.
        assert_eq!(below, ["Hot Dog Stand"]);
    }

    /// The light-theme promise `theme.rs` tests for the curated themes:
    /// a dark-oriented program's bright text is corrected to stay readable.
    #[test]
    fn vendored_light_themes_keep_dark_oriented_text_readable() {
        use diri_proto::grid::{GridCell, TermColor, TermStyle};

        use crate::contrast::{linear, oklch};
        let mut mid_tone = 0;
        for theme in catalog()
            .iter()
            .filter(|theme| theme.appearance == ThemeAppearance::Light)
        {
            for foreground in [TermColor::Rgb(255, 255, 255), TermColor::Ansi(15)] {
                let resolved = theme.resolve_cell(GridCell::new(
                    u32::from('x'),
                    foreground,
                    TermColor::Default,
                    TermStyle::empty(),
                ));
                let ratio = contrast_ratio(resolved.foreground, resolved.background);
                let paper = oklch(linear(theme.background)).lightness;
                if paper >= 0.85 {
                    assert!(ratio >= 4.5, "{}: {ratio}", theme.name);
                } else {
                    // Mid-tone paper phases correction in (see `contrast`);
                    // it may only ever help.
                    let authored = theme.resolve_color(foreground, false);
                    let before = contrast_ratio(authored, resolved.background);
                    assert!(ratio >= before, "{}: {ratio} < {before}", theme.name);
                    mid_tone += 1;
                }
            }
        }
        eprintln!("mid-tone light papers: {}", mid_tone / 2);
    }

    #[test]
    fn vendored_tints_match_the_derivation() {
        let mut stale = Vec::new();
        for row in rows() {
            let tints = spec(&row).tints();
            let stored = [row.colors[5], row.colors[6], row.colors[7]];
            let derived = [tints.selection, tints.find_match, tints.find_match_current];
            // One 8-bit step absorbs libm differences between platforms.
            let off = stored.iter().zip(&derived).any(|(left, right)| {
                [
                    left.r - right.r,
                    left.g - right.g,
                    left.b - right.b,
                    left.a - right.a,
                ]
                .iter()
                .any(|delta| delta.abs() > 1.01 / 255.0)
            });
            if off {
                stale.push(row.name);
            }
        }
        assert!(
            stale.is_empty(),
            "stale tints for {stale:?}; rerun regenerate_vendored_catalog"
        );
    }

    #[test]
    fn imported_themes_live_beside_the_catalog() {
        let theme = TermTheme {
            id: intern("imported-test-only-theme"),
            name: intern("Test Only Theme"),
            ..TermTheme::DRACULA
        };
        add_imported(theme);
        assert_eq!(find(theme.id), Some(theme));
        assert!(all().iter().any(|candidate| candidate.id == theme.id));
        assert!(is_imported(theme.id));
        assert!(std::ptr::eq(intern("Test Only Theme"), theme.name));
    }

    fn tint_literal(color: Rgba) -> String {
        let byte = |channel: f32| (channel * 255.0).round() as u8;
        format!(
            "{:02x}{:02x}{:02x}{:02x}",
            byte(color.r),
            byte(color.g),
            byte(color.b),
            byte(color.a)
        )
    }

    /// Rebuilds `themes/iterm2-color-schemes.tsv` from an upstream checkout:
    ///
    /// ```sh
    /// DIRI_THEME_SOURCE=/path/to/iTerm2-Color-Schemes \
    ///   cargo test -p diri-term regenerate_vendored_catalog -- --ignored
    /// ```
    #[test]
    #[ignore = "needs an iTerm2-Color-Schemes checkout in DIRI_THEME_SOURCE"]
    fn regenerate_vendored_catalog() {
        let source = std::path::PathBuf::from(
            std::env::var_os("DIRI_THEME_SOURCE").expect("set DIRI_THEME_SOURCE"),
        );
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default();
        let mut entries = std::fs::read_dir(source.join("ghostty"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries
            .sort_by_cached_key(|path| path.file_name().unwrap().to_string_lossy().to_lowercase());

        let mut out = String::new();
        out.push_str(
            "# Terminal themes from iTerm2-Color-Schemes, https://github.com/mbadolato/iTerm2-Color-Schemes\n",
        );
        out.push_str(&format!("# Upstream commit {commit}.\n"));
        out.push_str(
            "# The collection is MIT licensed (Copyright (c) 2011 to Present Mark Badolato);\n\
             # each theme's copyright belongs to its author. See NOTICE at the repository root.\n\
             # Generated by diri-term `regenerate_vendored_catalog`; do not edit by hand.\n\
             # name<TAB>background foreground cursor cursor_text selection_seed selection find current ansi0..15\n",
        );
        let (mut kept, mut skipped) = (0, Vec::new());
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            let spec = crate::theme_file::parse_ghostty(&name, &text).unwrap();
            if crate::theme_file::body_contrast(&spec) < MINIMUM_BODY_CONTRAST
                || name.contains('\t')
            {
                skipped.push(name);
                continue;
            }
            let tints = spec.tints();
            let opaque =
                |color: Rgba| hex_string(composite(color, spec.background))[1..].to_owned();
            let mut fields = vec![
                opaque(spec.background),
                opaque(spec.foreground),
                opaque(spec.cursor.unwrap_or(spec.foreground)),
                opaque(spec.cursor_text.unwrap_or(spec.background)),
                opaque(spec.selection_seed()),
                tint_literal(tints.selection),
                tint_literal(tints.find_match),
                tint_literal(tints.find_match_current),
            ];
            fields.extend(spec.ansi.iter().map(|&color| opaque(color)));
            out.push_str(&format!("{name}\t{}\n", fields.join(" ")));
            kept += 1;
        }
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("themes/iterm2-color-schemes.tsv");
        std::fs::write(&target, out).unwrap();
        eprintln!(
            "kept {kept}, skipped {} for low contrast: {skipped:?}",
            skipped.len()
        );
    }
}
