//! User-facing desktop vocabulary that genuinely varies by operating system.

pub const fn local_machine_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else if cfg!(windows) {
        "This PC"
    } else {
        "This computer"
    }
}

pub const fn local_machine_label_lowercase() -> &'static str {
    if cfg!(target_os = "macos") {
        "this Mac"
    } else if cfg!(windows) {
        "this PC"
    } else {
        "this computer"
    }
}

/// The button that opens a folder in the system file manager.
pub const fn reveal_in_file_manager_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else if cfg!(windows) {
        "Show in File Explorer"
    } else {
        "Show in Folder"
    }
}

/// Leaving the app: macOS quits, Windows exits.
pub const fn quit_label() -> &'static str {
    if cfg!(windows) { "Exit Diri" } else { "Quit Diri" }
}

/// Whether the modifier diri's shortcuts, multi-select and link clicks use is
/// down: ⌘ on macOS, Ctrl elsewhere (`commands::linux_chord`). GPUI's
/// `platform` modifier is the Windows key on Windows, which opens Start.
pub fn shortcut_modifier(modifiers: &gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}
