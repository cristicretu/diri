//! Themes the person imported, kept as diri theme files in a `themes` folder
//! beside prefs.json (Application Support on macOS).
//!
//! An import is converted once, from whichever format it came in, to a diri
//! theme file holding the author's colors exactly; reading the folder back is
//! the only way themes enter the registry at startup. A theme's id is
//! [`themes::IMPORTED_PREFIX`] plus its file stem, so the selected theme
//! survives restarts and re-importing a theme of the same name replaces it.

use std::path::{Path, PathBuf};

use diri_term::theme::TermTheme;
use diri_term::theme_file::{self, DIRI_THEME_EXTENSION, ThemeFileError, ThemeSpec};
use diri_term::themes;

use crate::store::Prefs;

const DIR_NAME: &str = "themes";

/// The themes folder that goes with a prefs file.
pub(crate) fn dir_for(prefs_path: &Path) -> PathBuf {
    prefs_path
        .parent()
        .map_or_else(|| PathBuf::from(DIR_NAME), |parent| parent.join(DIR_NAME))
}

pub(crate) fn default_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_DIR.with(|dir| dir.borrow().clone()) {
        return dir;
    }
    dir_for(&Prefs::path())
}

#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Points [`default_dir`] at `dir` on this thread, so UI tests never write
/// into the person's real themes folder.
#[cfg(test)]
pub(crate) fn set_test_dir(dir: Option<PathBuf>) {
    TEST_DIR.with(|slot| *slot.borrow_mut() = dir);
}

/// Reads every theme in `dir`. Files that no longer parse are skipped; they
/// stay on disk for the person to fix.
pub(crate) fn load(dir: &Path) -> Vec<TermTheme> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut loaded = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().and_then(|extension| extension.to_str()) != Some(DIRI_THEME_EXTENSION) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let parsed = std::fs::read(&path)
            .map_err(|_| ThemeFileError::NotText)
            .and_then(|bytes| theme_file::parse(stem, &bytes));
        match parsed {
            Ok(spec) => loaded.push(build(stem, &spec)),
            Err(error) => {
                diri_telemetry::warn_event!("theme.import_unreadable", reason = error_kind(&error))
            }
        }
    }
    loaded
}

/// Startup: put the folder's themes in the registry.
pub(crate) fn load_into_registry(dir: &Path) {
    let loaded = load(dir);
    if !loaded.is_empty() {
        themes::set_imported(loaded);
    }
}

#[derive(Debug)]
pub(crate) enum ImportError {
    Read,
    Parse(ThemeFileError),
    Write,
}

/// Converts `bytes` (any supported format) into the themes folder and the
/// registry. `file_name` names themes whose format carries no name.
pub(crate) fn import(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<TermTheme, ImportError> {
    let spec = theme_file::parse(&theme_file::name_from_file_name(file_name), bytes)
        .map_err(ImportError::Parse)?;
    if spec.name.is_empty() {
        return Err(ImportError::Parse(ThemeFileError::Invalid(
            "theme has no name".into(),
        )));
    }
    let stem = match themes::slug(&spec.name) {
        slug if slug.is_empty() => "theme".to_owned(),
        slug => slug,
    };
    diri_platform::private::create_dir_all(dir).map_err(|_| ImportError::Write)?;
    let target = dir.join(format!("{stem}.{DIRI_THEME_EXTENSION}"));
    let temporary = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    diri_platform::private::replace_via(&temporary, &target, theme_file::write_diri(&spec))
        .map_err(|_| ImportError::Write)?;
    let theme = build(&stem, &spec);
    themes::add_imported(theme);
    Ok(theme)
}

pub(crate) fn import_path(dir: &Path, path: &Path) -> Result<TermTheme, ImportError> {
    let metadata = std::fs::metadata(path).map_err(|_| ImportError::Read)?;
    if metadata.len() > theme_file::MAX_THEME_FILE_BYTES as u64 {
        return Err(ImportError::Parse(ThemeFileError::TooLarge));
    }
    let bytes = std::fs::read(path).map_err(|_| ImportError::Read)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Imported");
    import(dir, file_name, &bytes)
}

/// A fixed label for telemetry; the error's own text can quote the file.
fn error_kind(error: &ThemeFileError) -> &'static str {
    match error {
        ThemeFileError::TooLarge => "too_large",
        ThemeFileError::NotText => "not_text",
        ThemeFileError::Invalid(_) => "invalid",
        ThemeFileError::Missing(_) => "missing",
        ThemeFileError::UnsupportedVersion(_) => "newer",
    }
}

fn build(stem: &str, spec: &ThemeSpec) -> TermTheme {
    let id = themes::intern(&format!("{}{stem}", themes::IMPORTED_PREFIX));
    spec.into_theme(id, themes::intern(&spec.name))
}

/// The file name an export of `theme` suggests.
pub(crate) fn export_file_name(theme: &TermTheme) -> String {
    let stem = themes::slug(theme.name);
    format!(
        "{}.{DIRI_THEME_EXTENSION}",
        if stem.is_empty() { "theme" } else { &stem }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_term::theme::ThemeAppearance;

    const GHOSTTY: &[u8] = include_bytes!("../../diri-term/tests/fixtures/themes/Flexoki Light");
    const ITERM: &[u8] =
        include_bytes!("../../diri-term/tests/fixtures/themes/Dracula.itermcolors");

    #[test]
    fn imports_persist_and_reload_with_the_same_id_and_colors() {
        let temp = tempfile::tempdir().unwrap();
        let dir = dir_for(&temp.path().join("prefs.json"));
        let theme = import(&dir, "Flexoki Light", GHOSTTY).unwrap();
        assert_eq!(theme.id, "imported-flexoki-light");
        assert_eq!(theme.name, "Flexoki Light");
        assert_eq!(theme.appearance, ThemeAppearance::Light);
        assert_eq!(themes::find(theme.id), Some(theme));
        assert!(dir.join("flexoki-light.diritheme").is_file());

        let reloaded = load(&dir);
        assert_eq!(reloaded, [theme]);
    }

    #[test]
    fn itermcolors_imports_take_the_file_name_and_reimports_replace() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("themes");
        let first = import(&dir, "My Dracula.itermcolors", ITERM).unwrap();
        assert_eq!(first.id, "imported-my-dracula");
        let again = import(&dir, "My Dracula.itermcolors", ITERM).unwrap();
        assert_eq!(first, again);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn exported_themes_import_back() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("themes");
        let text = theme_file::export_diri(&TermTheme::NORD);
        let theme = import(&dir, &export_file_name(&TermTheme::NORD), text.as_bytes()).unwrap();
        assert_eq!(theme.id, "imported-nord");
        assert_eq!(theme.name, "Nord");
        assert_eq!(theme.background, TermTheme::NORD.background);
        assert_eq!(theme.ansi, TermTheme::NORD.ansi);
    }

    #[test]
    fn bad_files_leave_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("themes");
        assert!(matches!(
            import(&dir, "broken", b"background = nope\n"),
            Err(ImportError::Parse(_))
        ));
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().count() == 0);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("junk.diritheme"), "{").unwrap();
        assert!(load(&dir).is_empty());
    }
}
