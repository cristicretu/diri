//! User-facing desktop vocabulary that genuinely varies by operating system.

pub fn local_machine_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This computer"
    }
}

pub fn local_machine_label_lowercase() -> &'static str {
    if cfg!(target_os = "macos") {
        "this Mac"
    } else {
        "this computer"
    }
}
