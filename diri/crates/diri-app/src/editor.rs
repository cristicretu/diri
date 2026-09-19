//! Hands a project folder to the user's editor. The command palette offers
//! "Open project in editor"; the first editor found here takes the folder.
use std::path::Path;
use std::process::Command;

/// Editors tried in order. `open -a` finds them by bundle name, so the app's
/// minimal launch PATH never matters.
#[cfg(target_os = "macos")]
const EDITOR_APPS: [&str; 3] = ["Cursor", "Visual Studio Code", "Zed"];

/// Editor command names tried in order on hosts without `open -a`.
#[cfg(not(target_os = "macos"))]
const EDITOR_COMMANDS: [&str; 3] = ["cursor", "code", "zed"];

/// Open `path` in the first editor that launches. Blocking on `open`
/// returns within milliseconds, so callers run this off the UI thread.
pub fn open_in_editor(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        for app in EDITOR_APPS {
            let launched = Command::new("open")
                .arg("-a")
                .arg(app)
                .arg(path)
                .status()
                .is_ok_and(|status| status.success());
            if launched {
                return Ok(());
            }
        }
        // No editor was found; Finder still shows the folder.
        Command::new("open")
            .arg(path)
            .status()
            .ok()
            .filter(|status| status.success())
            .map(|_| ())
            .ok_or_else(|| format!("Could not open {}", path.display()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        for command in EDITOR_COMMANDS {
            if Command::new(command).arg(path).spawn().is_ok() {
                return Ok(());
            }
        }
        Err(format!(
            "No editor found for {} (tried {})",
            path.display(),
            EDITOR_COMMANDS.join(", ")
        ))
    }
}
