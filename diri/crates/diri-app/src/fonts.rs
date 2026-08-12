//! Runtime font-family selection.
//!
//! Font choices are made once from GPUI's discovered catalog. macOS keeps its
//! virtual system family; Linux prefers common desktop and monospace families
//! while retaining fontconfig generic fallbacks for minimal installations.

use std::collections::HashSet;
use std::sync::OnceLock;

use gpui::{App, Font, FontFallbacks, font};

static UI_FAMILY: OnceLock<&'static str> = OnceLock::new();
static MONO_FAMILY: OnceLock<&'static str> = OnceLock::new();

/// Call once at startup, after GPUI has discovered the system font catalog.
pub fn init(cx: &App) {
    let names: HashSet<String> = cx.text_system().all_font_names().into_iter().collect();
    let _ = UI_FAMILY.set(select_ui(&names));
    let _ = MONO_FAMILY.set(select_mono(&names));
}

pub fn ui_family() -> &'static str {
    UI_FAMILY.get().copied().unwrap_or(default_ui())
}

pub fn mono_family() -> &'static str {
    MONO_FAMILY.get().copied().unwrap_or(default_mono())
}

pub fn terminal_font() -> Font {
    let mut mono = font(mono_family());
    mono.fallbacks = Some(FontFallbacks::from_fonts(terminal_fallbacks()));
    mono
}

#[cfg(target_os = "macos")]
fn default_ui() -> &'static str {
    ".SystemUIFont"
}

#[cfg(not(target_os = "macos"))]
fn default_ui() -> &'static str {
    "sans-serif"
}

#[cfg(target_os = "macos")]
fn default_mono() -> &'static str {
    "Menlo"
}

#[cfg(not(target_os = "macos"))]
fn default_mono() -> &'static str {
    "monospace"
}

#[cfg(target_os = "macos")]
fn select_ui(_names: &HashSet<String>) -> &'static str {
    default_ui()
}

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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

#[cfg(target_os = "macos")]
fn terminal_fallbacks() -> Vec<String> {
    [
        ".SF NS Mono",
        "Menlo",
        "Apple Symbols",
        "STIX Two Math",
        "Apple Color Emoji",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(not(target_os = "macos"))]
fn terminal_fallbacks() -> Vec<String> {
    [
        "Noto Sans Mono",
        "DejaVu Sans Mono",
        "Noto Sans Symbols 2",
        "STIX Two Math",
        "Noto Color Emoji",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{default_mono, default_ui, select_mono, select_ui};
    use std::collections::HashSet;

    #[test]
    fn font_selection_uses_discovered_families_or_platform_fallbacks() {
        assert_eq!(select_ui(&HashSet::new()), default_ui());
        assert_eq!(select_mono(&HashSet::new()), default_mono());

        #[cfg(target_os = "macos")]
        assert_eq!(
            select_mono(&HashSet::from(["SF Mono".to_owned()])),
            "SF Mono"
        );

        #[cfg(not(target_os = "macos"))]
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
}
