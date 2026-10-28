//! The terminal font: the user's family, the fallback chain behind it, the
//! bundled symbol font and the ligature setting.
//!
//! Prompts, file managers and editor plugins draw icons from the Nerd Font
//! Private Use Area (U+E000–U+F8FF, U+F0000 and up). Without a patched font
//! installed every one of them was a missing-glyph box, so Diri bundles the
//! official Symbols Nerd Font Mono, reduced to its Private Use Area glyphs and
//! scaled into one cell (`fonts/make_symbols_font.py`). It sits behind the
//! user's family: a patched family still draws its own icons, and the bundle
//! maps no code point outside the Private Use Area, so it never replaces a
//! glyph another font already draws.

use gpui::{App, Font, FontFallbacks, FontFeatures, font};

/// The family name of the bundled symbol font.
pub const SYMBOLS_FAMILY: &str = "Diri Symbols Mono";

/// `DiriSymbolsMono-Regular.ttf`: 10,610 glyphs, 2.3 MB, mapped with the
/// binary and paged in only as glyphs are rasterized.
static SYMBOLS_FONT: &[u8] = include_bytes!("../fonts/DiriSymbolsMono-Regular.ttf");

/// Makes the bundled fonts resolvable by family name. Idempotent; call once
/// the text system exists and before the first terminal paints.
///
/// # Errors
/// When the platform rejects the font data.
pub fn register_bundled_fonts(cx: &App) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = cx;
        static REGISTERED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        REGISTERED
            .get_or_init(|| mac::register_process_font(SYMBOLS_FONT))
            .clone()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // A borrowed slice is referenced, not copied.
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(SYMBOLS_FONT)])
            .map_err(|error| format!("{error:#}"))
    }
}

/// Families consulted, in order, for a character the terminal family lacks.
///
/// The symbol font comes first so no system font's stray Private Use Area
/// glyph wins over it; it maps nothing else, so every other character falls
/// through to the same fonts as before.
#[must_use]
pub fn fallback_families() -> Vec<String> {
    #[cfg(target_os = "macos")]
    let platform = [
        ".SF NS Mono",
        "Menlo",
        "Apple Symbols",
        "STIX Two Math",
        "Apple Color Emoji",
    ];
    #[cfg(windows)]
    let platform = [
        "Cascadia Mono",
        "Consolas",
        "Segoe UI Symbol",
        "Segoe UI Emoji",
    ];
    #[cfg(not(any(target_os = "macos", windows)))]
    let platform = [
        "Noto Sans Mono",
        "DejaVu Sans Mono",
        "Noto Sans Symbols 2",
        "STIX Two Math",
        "Noto Color Emoji",
    ];
    std::iter::once(SYMBOLS_FAMILY)
        .chain(platform)
        .map(str::to_owned)
        .collect()
}

/// OpenType features for the ligature setting. On leaves the font's own
/// defaults (programming fonts ship their ligatures as `calt`, on by
/// default); off turns off contextual alternates and ligatures, so `=>` is
/// two glyphs in two cells.
#[must_use]
pub fn ligature_features(ligatures: bool) -> FontFeatures {
    if ligatures {
        FontFeatures::default()
    } else {
        FontFeatures(std::sync::Arc::new(vec![
            ("calt".into(), 0),
            ("liga".into(), 0),
            ("dlig".into(), 0),
        ]))
    }
}

/// `family` with the terminal fallback chain and the ligature setting.
#[must_use]
pub fn terminal_font(family: &str, ligatures: bool) -> Font {
    let mut terminal = font(family.to_owned());
    terminal.fallbacks = Some(FontFallbacks::from_fonts(fallback_families()));
    terminal.features = ligature_features(ligatures);
    terminal
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDataProviderCreateWithData(
            info: *mut c_void,
            data: *const c_void,
            size: usize,
            release: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
        ) -> CFTypeRef;
        fn CGFontCreateWithDataProvider(provider: CFTypeRef) -> CFTypeRef;
    }

    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        fn CTFontManagerRegisterGraphicsFont(font: CFTypeRef, error: *mut CFTypeRef) -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: CFTypeRef);
    }

    /// Registers `data` for this process only, so CoreText's cascade lists
    /// find it by family name. Nothing is installed; the registration ends
    /// with the process.
    pub(super) fn register_process_font(data: &'static [u8]) -> Result<(), String> {
        // SAFETY: `data` is 'static, so the provider may reference it without
        // a release callback, and each created object is released once.
        unsafe {
            let provider = CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                data.as_ptr().cast(),
                data.len(),
                None,
            );
            if provider.is_null() {
                return Err("no data provider".to_owned());
            }
            let font = CGFontCreateWithDataProvider(provider);
            CFRelease(provider);
            if font.is_null() {
                return Err("unreadable font data".to_owned());
            }
            let mut error: CFTypeRef = std::ptr::null();
            let registered = CTFontManagerRegisterGraphicsFont(font, &raw mut error);
            CFRelease(font);
            if !error.is_null() {
                CFRelease(error);
            }
            if registered {
                Ok(())
            } else {
                Err("CoreText refused the registration".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_is_the_renamed_truetype_file() {
        // The generator checks the cmap; this guards against a swapped or
        // truncated file.
        assert!(SYMBOLS_FONT.len() > 2_000_000, "{}", SYMBOLS_FONT.len());
        assert_eq!(&SYMBOLS_FONT[..4], &[0, 1, 0, 0], "TrueType outlines");
        let family: Vec<u8> = SYMBOLS_FAMILY
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert!(
            SYMBOLS_FONT
                .windows(family.len())
                .any(|window| window == family),
            "the name table carries the renamed family"
        );
    }

    #[test]
    fn symbol_font_leads_the_fallback_chain_and_the_platform_chain_is_kept() {
        let chain = fallback_families();
        assert_eq!(chain[0], SYMBOLS_FAMILY);
        assert!(chain.len() >= 5);
        assert!(chain[1..].iter().all(|family| family != SYMBOLS_FAMILY));
    }

    #[test]
    fn ligatures_off_disables_contextual_alternates_and_ligatures() {
        assert!(ligature_features(true).tag_value_list().is_empty());
        let off = ligature_features(false);
        for tag in ["calt", "liga", "dlig"] {
            assert!(
                off.tag_value_list()
                    .iter()
                    .any(|(name, value)| name == tag && *value == 0),
                "{tag}"
            );
        }
        assert_ne!(terminal_font("Menlo", true), terminal_font("Menlo", false));
    }
}
