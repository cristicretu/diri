//! Runtime font-family selection.
//!
//! Font choices are made once from GPUI's discovered catalog. macOS keeps its
//! virtual system family; Linux prefers common desktop and monospace families
//! while retaining fontconfig generic fallbacks for minimal installations.
//!
//! The terminal family is the one user choice here. It is stored by name and
//! only honoured while that family is installed: GPUI substitutes a
//! proportional UI font for a missing family, which would wreck the grid.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use gpui::{App, Font, FontFallbacks};

static UI_FAMILY: OnceLock<&'static str> = OnceLock::new();
static MONO_FAMILY: OnceLock<&'static str> = OnceLock::new();
/// Families known to be installed: the catalog at launch plus whatever a
/// later [`monospace_families`] listing found (fonts installed since).
static INSTALLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Call once at startup, after GPUI has discovered the system font catalog.
pub fn init(cx: &App) {
    let names: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    let _ = UI_FAMILY.set(select_ui(&names));
    let _ = MONO_FAMILY.set(select_mono(&names));
    *installed() = Some(names);
    if let Err(error) = diri_term::glyphs::register_bundled_fonts(cx) {
        eprintln!("diri: bundled symbol font unavailable: {error}");
    }
}

fn installed() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    INSTALLED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn ui_family() -> &'static str {
    UI_FAMILY.get().copied().unwrap_or(default_ui())
}

pub fn mono_family() -> &'static str {
    MONO_FAMILY.get().copied().unwrap_or(default_mono())
}

/// The family the terminal actually paints with for a stored preference:
/// the preference while it is installed, otherwise the platform default.
pub fn terminal_family(configured: &str) -> &str {
    let usable = !configured.is_empty()
        && installed()
            .as_ref()
            .is_some_and(|names| names.contains(configured));
    if usable { configured } else { mono_family() }
}

/// The installed family a configured name (a family, or a PostScript face
/// name such as `JetBrainsMono-Regular`) means, if any.
pub fn installed_family(name: &str) -> Option<String> {
    let installed = installed();
    let names = installed.as_ref()?;
    crate::terminal_import::match_font_family(name, names.iter().map(String::as_str))
}

/// The terminal font for a stored family preference (empty = default) and
/// the ligature preference, in the Regular face.
pub fn terminal_font(configured: &str, ligatures: bool) -> Font {
    terminal_font_weighted(configured, ligatures, 400)
}

/// The terminal font the preferences ask for: family, ligatures and the
/// regular face's weight.
pub fn terminal_font_from(prefs: &crate::store::Prefs) -> Font {
    terminal_font_weighted(
        &prefs.terminal_font_family,
        prefs.terminal_ligatures,
        prefs.terminal_font_weight,
    )
}

/// The faces bold and italic cells use.
pub fn terminal_faces(prefs: &crate::store::Prefs) -> diri_term::glyphs::TerminalFaces {
    diri_term::glyphs::TerminalFaces {
        bold: prefs.terminal_bold_weight,
        italic: prefs.terminal_italic_weight,
    }
}

/// Font smoothing and stroke thickening for terminal glyphs.
pub fn glyph_raster(prefs: &crate::store::Prefs) -> gpui::GlyphRasterStyle {
    gpui::GlyphRasterStyle {
        smoothing: prefs.terminal_font_smoothing,
        stroke: prefs
            .terminal_font_thicken
            .min(gpui::GlyphRasterStyle::MAX_STROKE),
    }
}

/// [`terminal_font`] at a regular-face `weight`. Called on every terminal
/// frame, so the last answer is reused.
pub fn terminal_font_weighted(configured: &str, ligatures: bool, weight: u16) -> Font {
    thread_local! {
        static LAST: RefCell<Option<(String, bool, u16, Font)>> = const { RefCell::new(None) };
    }
    let family = terminal_family(configured);
    LAST.with_borrow_mut(|last| {
        if let Some((key, key_ligatures, key_weight, font)) = last.as_ref()
            && key == family
            && *key_ligatures == ligatures
            && *key_weight == weight
        {
            return font.clone();
        }
        let mut mono = diri_term::glyphs::terminal_font_with_weight(family, ligatures, weight);
        let mut fallbacks = diri_term::glyphs::fallback_families();
        // A custom family may lack glyphs the default covers. The bundled
        // symbol font stays first: it only maps the Private Use Area.
        if mono.family.as_ref() != mono_family() {
            fallbacks.insert(1, mono_family().to_owned());
        }
        mono.fallbacks = Some(FontFallbacks::from_fonts(fallbacks));
        *last = Some((family.to_owned(), ligatures, weight, mono.clone()));
        mono
    })
}

/// One face of the terminal family, as the face pickers offer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontFace {
    /// CSS weight, rounded; font selection snaps it back onto the face.
    pub weight: u16,
    pub italic: bool,
}

/// The faces of the family `prefs` paints with. Reads the family GPUI has
/// already loaded to paint, so it is cheap after the first terminal frame.
pub fn font_faces(cx: &App, prefs: &crate::store::Prefs) -> Vec<FontFace> {
    let font = terminal_font(&prefs.terminal_font_family, prefs.terminal_ligatures);
    faces_from(cx.text_system().font_faces(&font))
}

/// Rounds, sorts and de-duplicates a family's faces. A platform that cannot
/// list faces gets the four every family answers to.
fn faces_from(raw: Vec<(gpui::FontWeight, gpui::FontStyle)>) -> Vec<FontFace> {
    let mut faces: Vec<FontFace> = raw
        .into_iter()
        .filter(|(weight, _)| weight.0.is_finite())
        .map(|(weight, style)| FontFace {
            weight: weight.0.round().clamp(1.0, 1000.0) as u16,
            italic: style != gpui::FontStyle::Normal,
        })
        .collect();
    if faces.is_empty() {
        faces = [(400, false), (700, false), (400, true), (700, true)]
            .into_iter()
            .map(|(weight, italic)| FontFace { weight, italic })
            .collect();
    }
    faces.sort_by_key(|face| (face.italic, face.weight));
    faces.dedup();
    faces
}

/// The faces a picker lists: upright ones for Regular and Bold, italic ones
/// for Italic. A family with no upright face (or none slanted) offers all.
pub fn faces_for_picker(faces: &[FontFace], italic: bool) -> Vec<u16> {
    let mut weights: Vec<u16> = faces
        .iter()
        .filter(|face| face.italic == italic)
        .map(|face| face.weight)
        .collect();
    if weights.is_empty() && !italic {
        weights = faces.iter().map(|face| face.weight).collect();
        weights.sort_unstable();
        weights.dedup();
    }
    weights
}

/// The localized name of a weight: Thin, Light, Regular, Medium, Bold…
/// Halfway weights take the lighter name: CoreText reports Thin faces at
/// 150, between Thin and Extra Light.
pub fn weight_label(weight: u16) -> &'static str {
    use crate::i18n::t;
    match weight.saturating_add(49) / 100 {
        0 | 1 => t("settings.appearance.weight_100"),
        2 => t("settings.appearance.weight_200"),
        3 => t("settings.appearance.weight_300"),
        4 => t("settings.appearance.weight_400"),
        5 => t("settings.appearance.weight_500"),
        6 => t("settings.appearance.weight_600"),
        7 => t("settings.appearance.weight_700"),
        8 => t("settings.appearance.weight_800"),
        _ => t("settings.appearance.weight_900"),
    }
}

/// Installed fixed-pitch families, sorted, without private (`.`) families.
/// Reads the live font catalog, so call it when a picker opens, not per
/// frame. The returned listing does the reading and is meant for a
/// background thread: a Mac with a few hundred monospaced faces (Nerd Font
/// collections) took seconds over it on the main thread.
pub fn monospace_families(cx: &App) -> impl FnOnce() -> Vec<String> + Send + 'static {
    let list = platform_monospace_families(cx);
    move || {
        let mut families = list();
        families.retain(|family| !family.is_empty() && !family.starts_with('.'));
        families.sort_by_key(|family| family.to_lowercase());
        families.dedup();
        installed()
            .get_or_insert_with(HashSet::new)
            .extend(families.iter().cloned());
        families
    }
}

/// Matches font descriptors by trait and reads their family attribute, so no
/// face is instantiated (opening each one cost 2-4 ms of the main thread).
/// `NSFontDescriptor` is safe off the main thread, unlike `NSFontManager`.
#[cfg(target_os = "macos")]
fn platform_monospace_families(_cx: &App) -> impl FnOnce() -> Vec<String> + Send + 'static {
    monospace_descriptor_families
}

#[cfg(target_os = "macos")]
fn monospace_descriptor_families() -> Vec<String> {
    {
        use objc2::rc::{Retained, autoreleasepool};
        use objc2::runtime::AnyObject;
        use objc2_app_kit::{
            NSFontDescriptor, NSFontDescriptorSymbolicTraits, NSFontFamilyAttribute,
            NSFontSymbolicTrait, NSFontTraitsAttribute,
        };
        use objc2_foundation::{NSDictionary, NSNumber, NSSet, NSString};

        autoreleasepool(|_| {
            // SAFETY: AppKit's own attribute keys.
            let (traits_key, symbolic_key, family_key) = unsafe {
                (
                    NSFontTraitsAttribute,
                    NSFontSymbolicTrait,
                    NSFontFamilyAttribute,
                )
            };
            let monospace: Retained<AnyObject> =
                NSNumber::new_u32(NSFontDescriptorSymbolicTraits::TraitMonoSpace.0).into();
            let traits: Retained<AnyObject> =
                NSDictionary::<NSString, AnyObject>::from_retained_objects(
                    &[symbolic_key],
                    &[monospace],
                )
                .into();
            let attributes = NSDictionary::<NSString, AnyObject>::from_retained_objects(
                &[traits_key],
                &[traits],
            );
            // SAFETY: a traits dictionary holding a symbolic-trait number is
            // the documented shape of a font attributes query.
            let query =
                unsafe { NSFontDescriptor::fontDescriptorWithFontAttributes(Some(&attributes)) };
            let mandatory = NSSet::from_slice(&[traits_key]);
            let matches = query.matchingFontDescriptorsWithMandatoryKeys(Some(&mandatory));
            let mut families = HashSet::new();
            for descriptor in matches.iter() {
                if let Some(family) = descriptor
                    .objectForKey(family_key)
                    .and_then(|value| value.downcast::<NSString>().ok())
                {
                    families.insert(family.to_string());
                }
            }
            families.into_iter().collect()
        })
    }
}

/// Without a fixed-pitch query, offer the well-known coding families that
/// are installed.
#[cfg(not(target_os = "macos"))]
fn platform_monospace_families(cx: &App) -> impl FnOnce() -> Vec<String> + Send + 'static {
    let names: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    move || {
        [
            "monospace",
            "JetBrains Mono",
            "Fira Code",
            "Fira Mono",
            "Cascadia Code",
            "Cascadia Mono",
            "Source Code Pro",
            "Hack",
            "Iosevka",
            "IBM Plex Mono",
            "Ubuntu Mono",
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Inconsolata",
            "Roboto Mono",
            "Geist Mono",
        ]
        .into_iter()
        .filter(|family| names.contains(*family))
        .map(str::to_owned)
        .collect()
    }
}

#[cfg(target_os = "macos")]
fn default_ui() -> &'static str {
    ".SystemUIFont"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_ui() -> &'static str {
    "sans-serif"
}

#[cfg(target_os = "macos")]
fn default_mono() -> &'static str {
    "Menlo"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_mono() -> &'static str {
    "monospace"
}

#[cfg(target_os = "macos")]
fn select_ui(_names: &HashSet<String>) -> &'static str {
    default_ui()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn select_ui(names: &HashSet<String>) -> &'static str {
    ["Noto Sans", "DejaVu Sans", "Liberation Sans", "Ubuntu"]
        .into_iter()
        .find(|candidate| names.contains(*candidate))
        .unwrap_or(default_ui())
}

#[cfg(target_os = "macos")]
fn select_mono(names: &HashSet<String>) -> &'static str {
    if names.contains("SF Mono") {
        "SF Mono"
    } else {
        default_mono()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn select_mono(names: &HashSet<String>) -> &'static str {
    [
        "JetBrains Mono",
        "Cascadia Mono",
        "Noto Sans Mono",
        "DejaVu Sans Mono",
        "Liberation Mono",
    ]
    .into_iter()
    .find(|candidate| names.contains(*candidate))
    .unwrap_or(default_mono())
}



#[cfg(windows)]
fn default_ui() -> &'static str {
    "Segoe UI"
}
#[cfg(windows)]
fn default_mono() -> &'static str {
    "Consolas"
}
#[cfg(windows)]
fn select_ui(names: &HashSet<String>) -> &'static str {
    ["Segoe UI Variable", "Segoe UI"]
        .into_iter()
        .find(|name| names.contains(*name))
        .unwrap_or(default_ui())
}
#[cfg(windows)]
fn select_mono(names: &HashSet<String>) -> &'static str {
    ["Cascadia Mono", "Cascadia Code", "Consolas"]
        .into_iter()
        .find(|name| names.contains(*name))
        .unwrap_or(default_mono())
}

#[cfg(test)]
mod tests {
    use super::{
        FontFace, default_mono, default_ui, faces_for_picker, faces_from, installed, mono_family,
        select_mono, select_ui, terminal_family, terminal_font, terminal_font_weighted,
    };
    use std::collections::HashSet;

    #[test]
    fn terminal_family_is_honoured_only_while_installed() {
        installed()
            .get_or_insert_with(HashSet::new)
            .insert("Diri Test Mono".to_owned());
        assert_eq!(terminal_family(""), mono_family());
        assert_eq!(terminal_family("Diri Test Mono"), "Diri Test Mono");
        assert_eq!(terminal_family("Uninstalled Mono"), mono_family());

        let custom = terminal_font("Diri Test Mono", true);
        assert_eq!(custom.family.as_ref(), "Diri Test Mono");
        let chain = custom.fallbacks.as_ref().unwrap().fallback_list();
        assert_eq!(chain[0], diri_term::glyphs::SYMBOLS_FAMILY);
        assert_eq!(chain[1], mono_family());
        assert_eq!(terminal_font("", true).family.as_ref(), mono_family());
        // The ligature preference is part of the font, and the reused
        // answer follows it.
        assert!(terminal_font("", true).features.tag_value_list().is_empty());
        assert!(!terminal_font("", false).features.tag_value_list().is_empty());
        assert!(terminal_font("", true).features.tag_value_list().is_empty());
    }

    #[test]
    fn regular_weight_is_part_of_the_reused_font() {
        assert_eq!(terminal_font("", true).weight, gpui::FontWeight::NORMAL);
        assert_eq!(
            terminal_font_weighted("", true, 500).weight,
            gpui::FontWeight(500.0)
        );
        assert_eq!(terminal_font("", true).weight, gpui::FontWeight::NORMAL);
    }

    #[test]
    fn faces_are_rounded_sorted_and_split_by_slant() {
        use gpui::{FontStyle, FontWeight};
        let faces = faces_from(vec![
            (FontWeight(700.0), FontStyle::Normal),
            (FontWeight(529.97), FontStyle::Normal),
            (FontWeight(400.0), FontStyle::Italic),
            (FontWeight(400.0), FontStyle::Normal),
            (FontWeight(400.0), FontStyle::Normal),
            (FontWeight(300.0), FontStyle::Oblique),
        ]);
        assert_eq!(
            faces,
            [
                FontFace { weight: 400, italic: false },
                FontFace { weight: 530, italic: false },
                FontFace { weight: 700, italic: false },
                FontFace { weight: 300, italic: true },
                FontFace { weight: 400, italic: true },
            ]
        );
        assert_eq!(faces_for_picker(&faces, false), [400, 530, 700]);
        assert_eq!(faces_for_picker(&faces, true), [300, 400]);
        // An italic-only family still offers its faces as Regular.
        let italic_only = faces_from(vec![(FontWeight(400.0), FontStyle::Italic)]);
        assert_eq!(faces_for_picker(&italic_only, false), [400]);
        assert!(faces_for_picker(&faces_from(vec![(FontWeight(400.0), FontStyle::Normal)]), true).is_empty());
        // Halfway weights read as the lighter name.
        assert_eq!(super::weight_label(150), super::weight_label(100));
        assert_ne!(super::weight_label(150), super::weight_label(200));
        assert_eq!(super::weight_label(530), super::weight_label(500));
        // Nothing listed: Regular, Bold and their italics.
        assert_eq!(faces_from(Vec::new()).len(), 4);
    }

    #[test]
    fn font_selection_uses_discovered_families_or_platform_fallbacks() {
        assert_eq!(select_ui(&HashSet::new()), default_ui());
        assert_eq!(select_mono(&HashSet::new()), default_mono());

        #[cfg(target_os = "macos")]
        assert_eq!(
            select_mono(&HashSet::from(["SF Mono".to_owned()])),
            "SF Mono"
        );

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert_eq!(
                select_ui(&HashSet::from(["Noto Sans".to_owned()])),
                "Noto Sans"
            );
            assert_eq!(
                select_mono(&HashSet::from(["DejaVu Sans Mono".to_owned()])),
                "DejaVu Sans Mono"
            );
        }
    }

    /// The picker's listing runs on a background thread: it must work there
    /// and find the fixed-pitch families every Mac ships, without opening
    /// each face (the old main-thread listing spent 2-4 ms per face).
    #[cfg(target_os = "macos")]
    #[test]
    fn monospace_families_are_listed_off_the_main_thread() {
        use super::monospace_descriptor_families;
        let (families, took) = std::thread::spawn(|| {
            let started = std::time::Instant::now();
            let families = monospace_descriptor_families();
            (families, started.elapsed())
        })
        .join()
        .unwrap();
        for family in ["Menlo", "Monaco", "Courier New"] {
            assert!(
                families.iter().any(|f| f == family),
                "{family}: {families:?}"
            );
        }
        assert!(
            families.iter().all(|f| f != "Helvetica"),
            "proportional family listed: {families:?}"
        );
        eprintln!("{} monospace families in {took:?}", families.len());
    }
}
