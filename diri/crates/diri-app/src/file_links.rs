//! Terminal `file:line` references: which ones name a real local file, and
//! the URL that opens that place in the user's editor.
//!
//! Editors are reached through their URL schemes (`cursor://file/…:12:5`)
//! and `NSWorkspace`, never through their command-line tools: an app
//! launched from the Dock does not have the user's shell `PATH`, so `cursor`
//! or `code` would usually not be found, and spawning them would also mean
//! a process per click.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::code_intelligence::{SourceTarget, local_reference};
use crate::store::FileEditor;

/// An editor that can be asked, by URL, to show a file at a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Editor {
    Cursor,
    VsCode,
    Zed,
}

impl Editor {
    /// Automatic detection tries these in order.
    const PREFERENCE: [Self; 3] = [Self::Cursor, Self::VsCode, Self::Zed];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Cursor => "Cursor",
            Self::VsCode => "VS Code",
            Self::Zed => "Zed",
        }
    }

    const fn scheme(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::VsCode => "vscode",
            Self::Zed => "zed",
        }
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    const fn bundle_identifier(self) -> &'static str {
        match self {
            Self::Cursor => "com.todesktop.230313mzl4w4u92",
            Self::VsCode => "com.microsoft.VSCode",
            Self::Zed => "dev.zed.Zed",
        }
    }

    const fn from_choice(choice: FileEditor) -> Option<Self> {
        match choice {
            FileEditor::Cursor => Some(Self::Cursor),
            FileEditor::VsCode => Some(Self::VsCode),
            FileEditor::Zed => Some(Self::Zed),
            FileEditor::Automatic | FileEditor::DefaultApp => None,
        }
    }
}

/// The editor a choice resolves to; `None` means the file's default app.
pub(crate) fn editor_for(choice: FileEditor) -> Option<Editor> {
    match choice {
        FileEditor::Automatic => installed_editor(),
        FileEditor::DefaultApp => None,
        chosen => Editor::from_choice(chosen),
    }
}

/// The first installed editor in `Editor::PREFERENCE`, looked up once per
/// launch by bundle identifier.
fn installed_editor() -> Option<Editor> {
    static DETECTED: std::sync::OnceLock<Option<Editor>> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| {
        Editor::PREFERENCE
            .into_iter()
            .find(|editor| is_installed(*editor))
    })
}

#[cfg(target_os = "macos")]
fn is_installed(editor: Editor) -> bool {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;
    let identifier = NSString::from_str(editor.bundle_identifier());
    NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&identifier)
        .is_some()
}

#[cfg(not(target_os = "macos"))]
fn is_installed(_: Editor) -> bool {
    false
}

/// A reference that names something on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalFile {
    pub path: PathBuf,
    pub target: Option<SourceTarget>,
    pub is_dir: bool,
}

impl LocalFile {
    /// `path:line:column` as the terminal named it, for the clipboard.
    pub(crate) fn display(&self) -> String {
        let path = self.path.display();
        match self.target {
            Some(SourceTarget { line, column }) if column > 1 => {
                format!("{path}:{line}:{column}")
            }
            Some(SourceTarget { line, .. }) => format!("{path}:{line}"),
            None => path.to_string(),
        }
    }
}

/// Resolves `reference` against each base directory in turn and returns the
/// first that exists. Relative references from a shell are meant from its
/// live directory, but an Agent prints them from its launch directory.
pub(crate) fn resolve(bases: &[&Path], reference: &str) -> Option<LocalFile> {
    let mut tried = Vec::with_capacity(bases.len());
    for base in bases {
        let (path, target) = local_reference(base, reference)?;
        if tried.contains(&path) {
            continue;
        }
        if let Ok(metadata) = std::fs::metadata(&path) {
            return Some(LocalFile {
                path,
                target,
                is_dir: metadata.is_dir(),
            });
        }
        tried.push(path);
    }
    None
}

/// Media and documents open in their own app unless the terminal named a
/// line: a screenshot path printed by an Agent should still open in Preview.
fn opens_in_default_app(path: &Path) -> bool {
    const VIEWED: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "heic", "tiff", "bmp", "ico", "icns", "svg", "pdf",
        "html", "htm", "mp4", "mov", "m4v", "webm", "mp3", "wav", "m4a", "aiff", "zip", "dmg",
        "app",
    ];
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            VIEWED
                .iter()
                .any(|viewed| viewed.eq_ignore_ascii_case(extension))
        })
}

/// Whether handing `path` to the system opener would run something rather
/// than show it: an app or other bundle, a script or installer, a link file
/// that opens a URL, or anything marked executable.
pub(crate) fn runs_when_opened(path: &Path) -> bool {
    const RUNS: &[&str] = &[
        "app", "command", "tool", "terminal", "pkg", "mpkg", "dmg", "workflow", "action",
        "scpt", "scptd", "applescript", "jar", "sh", "zsh", "bash", "csh", "fish", "prefpane",
        "saver", "webloc", "inetloc", "fileloc", "url", "shortcut", "mobileconfig",
        "osax", "kext", "plugin", "bundle", "xpc", "systemextension", "appex",
    ];
    let named = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| RUNS.iter().any(|runs| runs.eq_ignore_ascii_case(extension)));
    if named {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            return metadata.is_file() && metadata.permissions().mode() & 0o111 != 0;
        }
    }
    false
}

/// What a click on `file` opens: the editor's own URL at the line when an
/// editor applies, otherwise the `file://` URL for the system opener.
pub(crate) fn open_url(file: &LocalFile, editor: Option<Editor>) -> Option<String> {
    let file_url = url::Url::from_file_path(&file.path).ok()?;
    let editor = editor
        .filter(|_| !file.is_dir && (file.target.is_some() || !opens_in_default_app(&file.path)));
    let Some(editor) = editor else {
        return Some(file_url.into());
    };
    // The file URL's path is already absolute and percent-encoded.
    let mut url = format!("{}://file{}", editor.scheme(), file_url.path());
    if let Some(SourceTarget { line, column }) = file.target {
        url.push_str(&format!(":{line}:{column}"));
    }
    Some(url)
}

/// Existence checks for the reference under the pointer. Hovering re-asks on
/// every repaint that moves the pointer or the output; the answer is reused
/// for a short while so that costs one `stat` per reference, not per frame,
/// yet a file created a moment later still becomes a link.
#[derive(Default)]
pub(crate) struct ExistenceCache {
    entries: HashMap<(Vec<PathBuf>, String), (Instant, Option<LocalFile>)>,
}

impl ExistenceCache {
    const FRESH_FOR: Duration = Duration::from_secs(2);
    const CAPACITY: usize = 256;

    pub(crate) fn resolve(
        &mut self,
        bases: &[&Path],
        reference: &str,
        now: Instant,
    ) -> Option<LocalFile> {
        let key = (
            bases.iter().map(|base| base.to_path_buf()).collect(),
            reference.to_owned(),
        );
        if let Some((checked, file)) = self.entries.get(&key)
            && now.saturating_duration_since(*checked) < Self::FRESH_FOR
        {
            return file.clone();
        }
        if self.entries.len() >= Self::CAPACITY {
            self.entries.retain(|_, (checked, _)| {
                now.saturating_duration_since(*checked) < Self::FRESH_FOR
            });
            if self.entries.len() >= Self::CAPACITY {
                self.entries.clear();
            }
        }
        let file = resolve(bases, reference);
        self.entries.insert(key, (now, file.clone()));
        file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/app.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.path().join("shot one.png"), b"png").unwrap();
        root
    }

    #[test]
    fn relative_references_resolve_from_each_base_in_order() {
        let launch = fixture();
        let live = tempfile::tempdir().unwrap();
        let bases = [live.path(), launch.path()];
        let file = resolve(&bases, "src/app.rs:42:9").expect("found from the launch dir");
        assert_eq!(file.path, launch.path().join("src/app.rs"));
        assert_eq!(
            file.target,
            Some(SourceTarget {
                line: 42,
                column: 9
            })
        );
        assert!(!file.is_dir);
        assert!(resolve(&bases, "src/missing.rs:1").is_none());
        let folder = launch.path().join("src").display().to_string();
        assert!(resolve(&[live.path()], &folder).unwrap().is_dir);
        let absolute = format!("{}:3", launch.path().join("src/app.rs").display());
        assert!(resolve(&[live.path()], &absolute).is_some());
    }

    #[test]
    fn editors_get_their_own_url_at_the_line() {
        let root = fixture();
        let file = resolve(&[root.path()], "src/app.rs:42:9").unwrap();
        let path = root.path().join("src/app.rs");
        let path = url::Url::from_file_path(&path).unwrap().path().to_owned();
        for (editor, scheme) in [
            (Editor::Cursor, "cursor"),
            (Editor::VsCode, "vscode"),
            (Editor::Zed, "zed"),
        ] {
            assert_eq!(
                open_url(&file, Some(editor)).unwrap(),
                format!("{scheme}://file{path}:42:9")
            );
        }
        let line_only = resolve(&[root.path()], "src/app.rs:7").unwrap();
        assert_eq!(
            open_url(&line_only, Some(Editor::Cursor)).unwrap(),
            format!("cursor://file{path}:7:1")
        );
        let bare = resolve(&[root.path()], "src/app.rs").unwrap();
        assert_eq!(
            open_url(&bare, Some(Editor::Zed)).unwrap(),
            format!("zed://file{path}")
        );
        assert_eq!(
            open_url(&file, None).unwrap(),
            format!("file://{path}"),
            "no editor: the system opener, which takes no line"
        );
    }

    #[test]
    fn media_and_folders_keep_the_system_opener_and_paths_are_encoded() {
        let root = fixture();
        let image = resolve(&[root.path()], "'shot one.png'").unwrap();
        let opened = open_url(&image, Some(Editor::Cursor)).unwrap();
        assert!(opened.starts_with("file://"), "{opened}");
        assert!(opened.ends_with("/shot%20one.png"), "{opened}");
        let folder = root.path().join("src").display().to_string();
        let folder = resolve(&[root.path()], &folder).unwrap();
        assert!(
            open_url(&folder, Some(Editor::Cursor))
                .unwrap()
                .starts_with("file://")
        );
        // A line number means the user wants the source, even of an image.
        let svg_line = LocalFile {
            path: root.path().join("icon.svg"),
            target: Some(SourceTarget { line: 3, column: 1 }),
            is_dir: false,
        };
        assert!(
            open_url(&svg_line, Some(Editor::VsCode))
                .unwrap()
                .starts_with("vscode://file/")
        );
    }

    #[test]
    fn explicit_choices_never_detect_and_default_app_has_no_editor() {
        assert_eq!(editor_for(FileEditor::Cursor), Some(Editor::Cursor));
        assert_eq!(editor_for(FileEditor::VsCode), Some(Editor::VsCode));
        assert_eq!(editor_for(FileEditor::Zed), Some(Editor::Zed));
        assert_eq!(editor_for(FileEditor::DefaultApp), None);
    }

    #[test]
    fn existence_answers_are_reused_briefly_then_rechecked() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = ExistenceCache::default();
        let now = Instant::now();
        let bases = [root.path()];
        assert!(cache.resolve(&bases, "late.rs:1", now).is_none());
        std::fs::write(root.path().join("late.rs"), "").unwrap();
        assert!(
            cache
                .resolve(&bases, "late.rs:1", now + Duration::from_millis(500))
                .is_none(),
            "a fresh answer is reused"
        );
        assert!(
            cache
                .resolve(&bases, "late.rs:1", now + Duration::from_secs(3))
                .is_some(),
            "a stale one is checked again"
        );
    }

    #[test]
    fn runnable_files_are_told_from_documents() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("build");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# notes").unwrap();
        assert!(!runs_when_opened(&script));
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(runs_when_opened(&script), "executable bit");
        assert!(!runs_when_opened(&notes));
        for name in ["Tool.app", "x.command", "Setup.PKG", "link.webloc", "a.terminal"] {
            assert!(runs_when_opened(Path::new(name)), "{name}");
        }
        // A folder opens in Finder; only bundles run.
        assert!(!runs_when_opened(dir.path()));
    }
}
