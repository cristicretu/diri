//! Validation and staging for paths entering Diri from the desktop.
//!
//! Keep this module independent of GPUI and the sidebar. A desktop drop is
//! untrusted, transient input: Finder may hand us a vanished file, a cloud
//! placeholder, or a path the process cannot read. Callers receive a complete
//! plan and may update UI state from it, but this module never opens a session,
//! sends input, starts a download, or executes a path.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use diri_proto::SessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalPathKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StagedExternalPath {
    pub path: PathBuf,
    pub kind: ExternalPathKind,
    quoted: String,
}

impl StagedExternalPath {
    pub fn quoted(&self) -> &str {
        &self.quoted
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExternalPathRejection {
    Missing,
    Unreadable,
    NotMaterialized,
    NotFileOrDirectory,
    NotUnicode,
    RequiresDirectory,
    AdditionalPath,
    RemoteTarget,
    RemoteDirectory,
}

impl ExternalPathRejection {
    fn explanation(&self) -> &'static str {
        match self {
            Self::Missing => "does not exist",
            Self::Unreadable => "is not readable",
            Self::NotMaterialized => "is not downloaded to this Mac",
            Self::NotFileOrDirectory => "is not a file or directory",
            Self::NotUnicode => "cannot be represented in a prompt",
            Self::RequiresDirectory => "is not a directory",
            Self::AdditionalPath => "only the first directory starts the session",
            Self::RemoteTarget => "is local, but the target runs on another host",
            Self::RemoteDirectory => "is a folder, and only files can be sent to another host",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RejectedExternalPath {
    pub path: PathBuf,
    pub reason: ExternalPathRejection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExternalDropTarget {
    EmptySpace,
    Project { remote: bool },
    Session { id: SessionId, remote: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExternalDropAction {
    OpenLauncher {
        root: String,
    },
    OpenSessionComposer {
        session_id: SessionId,
        insertion: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalDropPlan {
    pub action: Option<ExternalDropAction>,
    pub rejected: Vec<RejectedExternalPath>,
}

impl ExternalDropPlan {
    pub fn accepts_drop(&self) -> bool {
        self.action.is_some()
    }

    /// Short, visible copy for the inline sidebar/composer feedback surface.
    pub fn feedback(&self) -> Option<String> {
        describe_rejections(self.action.is_some(), &self.rejected)
    }
}

/// One line naming up to three rejected paths and why, or `None` when every
/// path was accepted. `partial` picks the softer prefix used when the drop
/// still did something with the other paths.
fn describe_rejections(partial: bool, rejected: &[RejectedExternalPath]) -> Option<String> {
    if rejected.is_empty() {
        return None;
    }
    let prefix = if partial { "Ignored" } else { "Couldn't use" };
    let details = rejected
        .iter()
        .take(3)
        .map(|rejection| {
            format!(
                "“{}” {}",
                rejection.path.to_string_lossy(),
                rejection.reason.explanation()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let remaining = rejected.len().saturating_sub(3);
    Some(if remaining == 0 {
        format!("{prefix}: {details}.")
    } else {
        format!("{prefix}: {details}; and {remaining} more.")
    })
}

/// What a drop released directly on a terminal grid should do.
///
/// A terminal drop behaves like Terminal.app or iTerm2: the paths are typed
/// into the foreground program as one paste, which is how Claude Code, Codex
/// and Cursor pick up dropped images and attach them. Local sessions paste
/// immediately; sessions on another host first copy each file over so the
/// pasted path exists where the Agent runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDropAction {
    /// Paste this text into the PTY as one paste.
    Paste(String),
    /// Copy these files to the session's host, then paste the remote paths.
    Upload(Vec<PathBuf>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalDropPlan {
    pub action: Option<TerminalDropAction>,
    pub rejected: Vec<RejectedExternalPath>,
}

impl TerminalDropPlan {
    pub fn feedback(&self) -> Option<String> {
        describe_rejections(self.action.is_some(), &self.rejected)
    }
}

pub(crate) fn plan_terminal_drop(paths: &[PathBuf], remote: bool) -> TerminalDropPlan {
    plan_terminal_drop_with(paths, remote, &FileSystemProbe)
}

fn plan_terminal_drop_with(
    paths: &[PathBuf],
    remote: bool,
    probe: &impl PathProbe,
) -> TerminalDropPlan {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for result in stage_external_paths_with(paths, probe) {
        match result {
            Ok(path) if remote && path.kind == ExternalPathKind::Directory => {
                rejected.push(RejectedExternalPath {
                    path: path.path,
                    reason: ExternalPathRejection::RemoteDirectory,
                });
            }
            Ok(path) => accepted.push(path),
            Err(error) => rejected.push(error),
        }
    }
    let action = if accepted.is_empty() {
        None
    } else if remote {
        Some(TerminalDropAction::Upload(
            accepted.into_iter().map(|path| path.path).collect(),
        ))
    } else {
        Some(TerminalDropAction::Paste(terminal_drop_text(
            accepted
                .iter()
                .map(|path| path.path.to_str().expect("staged paths are unicode")),
        )))
    };
    TerminalDropPlan { action, rejected }
}

/// The text a desktop terminal pastes when files land on it: every path
/// backslash-escaped and followed by a space, matching Terminal.app and iTerm2.
/// Agents that attach dropped images (Claude Code, Codex) recognise exactly
/// this shape and strip the escapes themselves; the trailing space keeps a
/// second drop or typed text from fusing with the path.
pub(crate) fn terminal_drop_text<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut text = String::new();
    for path in paths {
        text.push_str(&escape_path_for_terminal(path));
        text.push(' ');
    }
    text
}

/// Backslash-escape a path the way macOS terminals do on drop. Alphanumerics,
/// non-ASCII text and the safe punctuation common in paths pass through;
/// everything a shell could interpret is escaped so the path survives even if
/// the foreground program is a plain shell.
pub(crate) fn escape_path_for_terminal(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        let plain = ch.is_alphanumeric()
            || !ch.is_ascii()
            || matches!(ch, '/' | '.' | '_' | '-' | '+' | ',' | ':' | '@');
        if !plain {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Validate and route a Finder payload for one sidebar target.
///
/// This function is intentionally safe to call for drag highlighting as well
/// as release: inspection is read-only, no contents are consumed, and dataless
/// files are rejected before `open` could ask the provider to materialize one.
pub(crate) fn plan_external_drop(
    paths: &[PathBuf],
    target: ExternalDropTarget,
) -> ExternalDropPlan {
    if let Some(plan) = remote_refusal(paths, &target) {
        return plan;
    }
    plan_staged_drop(target, stage_external_paths(paths))
}

#[cfg(test)]
fn plan_external_drop_with(
    paths: &[PathBuf],
    target: ExternalDropTarget,
    probe: &impl PathProbe,
) -> ExternalDropPlan {
    if paths.is_empty() {
        return ExternalDropPlan {
            action: None,
            rejected: Vec::new(),
        };
    }

    if let Some(plan) = remote_refusal(paths, &target) {
        return plan;
    }
    let staged = stage_external_paths_with(paths, probe);
    plan_staged_drop(target, staged)
}

fn remote_refusal(paths: &[PathBuf], target: &ExternalDropTarget) -> Option<ExternalDropPlan> {
    let remote = match target {
        ExternalDropTarget::EmptySpace => false,
        ExternalDropTarget::Project { remote } | ExternalDropTarget::Session { remote, .. } => {
            *remote
        }
    };
    remote.then(|| ExternalDropPlan {
        action: None,
        rejected: paths
            .iter()
            .cloned()
            .map(|path| RejectedExternalPath {
                path,
                reason: ExternalPathRejection::RemoteTarget,
            })
            .collect(),
    })
}

fn plan_staged_drop(
    target: ExternalDropTarget,
    staged: Vec<Result<StagedExternalPath, RejectedExternalPath>>,
) -> ExternalDropPlan {
    match target {
        ExternalDropTarget::Session { id, remote: false } => {
            let mut accepted = Vec::new();
            let mut rejected = Vec::new();
            for result in staged {
                match result {
                    Ok(path) => accepted.push(path),
                    Err(error) => rejected.push(error),
                }
            }
            let insertion = accepted
                .iter()
                .map(StagedExternalPath::quoted)
                .collect::<Vec<_>>()
                .join(" ");
            ExternalDropPlan {
                action: (!insertion.is_empty()).then_some(
                    ExternalDropAction::OpenSessionComposer {
                        session_id: id,
                        insertion,
                    },
                ),
                rejected,
            }
        }
        ExternalDropTarget::EmptySpace | ExternalDropTarget::Project { remote: false } => {
            launcher_plan(staged)
        }
        ExternalDropTarget::Project { remote: true }
        | ExternalDropTarget::Session { remote: true, .. } => {
            unreachable!("refused before staging")
        }
    }
}

fn launcher_plan(
    staged: Vec<Result<StagedExternalPath, RejectedExternalPath>>,
) -> ExternalDropPlan {
    let chosen = staged.iter().position(|result| {
        matches!(
            result,
            Ok(StagedExternalPath {
                kind: ExternalPathKind::Directory,
                ..
            })
        )
    });
    let mut rejected = Vec::new();
    let mut root = None;
    for (index, result) in staged.into_iter().enumerate() {
        match result {
            Err(error) => rejected.push(error),
            Ok(path) if Some(index) == chosen => {
                root = path.path.to_str().map(str::to_owned);
                if root.is_none() {
                    rejected.push(RejectedExternalPath {
                        path: path.path,
                        reason: ExternalPathRejection::NotUnicode,
                    });
                }
            }
            Ok(path) => rejected.push(RejectedExternalPath {
                reason: if path.kind == ExternalPathKind::Directory && chosen.is_some() {
                    ExternalPathRejection::AdditionalPath
                } else {
                    ExternalPathRejection::RequiresDirectory
                },
                path: path.path,
            }),
        }
    }
    ExternalDropPlan {
        action: root.map(|root| ExternalDropAction::OpenLauncher { root }),
        rejected,
    }
}

fn stage_path(
    path: &Path,
    probe: &impl PathProbe,
) -> Result<StagedExternalPath, RejectedExternalPath> {
    let reject = |reason| RejectedExternalPath {
        path: path.to_path_buf(),
        reason,
    };
    let inspected = probe.inspect(path).map_err(reject)?;
    if !inspected.materialized {
        return Err(reject(ExternalPathRejection::NotMaterialized));
    }
    if !probe.readable(path, inspected.kind) {
        return Err(reject(ExternalPathRejection::Unreadable));
    }
    let Some(text) = path.to_str() else {
        return Err(reject(ExternalPathRejection::NotUnicode));
    };
    Ok(StagedExternalPath {
        path: path.to_path_buf(),
        kind: inspected.kind,
        quoted: quote_path(text),
    })
}

/// Reusable desktop-ingress gate for future attachment types. Consumers can
/// apply their own file-kind policy after this has established existence,
/// local materialization, readability, and lossless prompt representation.
pub(crate) fn stage_external_paths(
    paths: &[PathBuf],
) -> Vec<Result<StagedExternalPath, RejectedExternalPath>> {
    stage_external_paths_with(paths, &FileSystemProbe)
}

fn stage_external_paths_with(
    paths: &[PathBuf],
    probe: &impl PathProbe,
) -> Vec<Result<StagedExternalPath, RejectedExternalPath>> {
    paths.iter().map(|path| stage_path(path, probe)).collect()
}

/// POSIX single-quote a path. A literal quote closes the quoted span, emits an
/// escaped quote, and reopens it (`'` -> `'\''`). Nothing in the resulting
/// text can become shell syntax if an Agent chooses to pass it to a shell.
pub(crate) fn quote_path(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

#[derive(Clone, Copy)]
struct InspectedPath {
    kind: ExternalPathKind,
    materialized: bool,
}

trait PathProbe {
    fn inspect(&self, path: &Path) -> Result<InspectedPath, ExternalPathRejection>;
    fn readable(&self, path: &Path, kind: ExternalPathKind) -> bool;
}

struct FileSystemProbe;

impl PathProbe for FileSystemProbe {
    fn inspect(&self, path: &Path) -> Result<InspectedPath, ExternalPathRejection> {
        let metadata = fs::metadata(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => ExternalPathRejection::Missing,
            io::ErrorKind::PermissionDenied => ExternalPathRejection::Unreadable,
            _ => ExternalPathRejection::Unreadable,
        })?;
        let kind = if metadata.is_file() {
            ExternalPathKind::File
        } else if metadata.is_dir() {
            ExternalPathKind::Directory
        } else {
            return Err(ExternalPathRejection::NotFileOrDirectory);
        };
        Ok(InspectedPath {
            kind,
            materialized: is_materialized(&metadata),
        })
    }

    fn readable(&self, path: &Path, kind: ExternalPathKind) -> bool {
        match kind {
            ExternalPathKind::File => File::open(path).is_ok(),
            ExternalPathKind::Directory => fs::read_dir(path).is_ok(),
        }
    }
}

#[cfg(target_os = "macos")]
fn is_materialized(metadata: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt as _;

    // `SF_DATALESS` is the kernel's filesystem-level marker for an object
    // whose contents are owned by a File Provider and absent locally. Checking
    // it before `File::open` avoids implicitly starting a cloud download.
    const SF_DATALESS: u32 = 0x4000_0000;
    metadata.st_flags() & SF_DATALESS == 0
}

#[cfg(not(target_os = "macos"))]
fn is_materialized(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct FakeProbe {
        paths: HashMap<PathBuf, Result<InspectedPath, ExternalPathRejection>>,
        unreadable: Vec<PathBuf>,
    }

    impl FakeProbe {
        fn file(mut self, path: &str) -> Self {
            self.paths.insert(
                path.into(),
                Ok(InspectedPath {
                    kind: ExternalPathKind::File,
                    materialized: true,
                }),
            );
            self
        }

        fn directory(mut self, path: &str) -> Self {
            self.paths.insert(
                path.into(),
                Ok(InspectedPath {
                    kind: ExternalPathKind::Directory,
                    materialized: true,
                }),
            );
            self
        }

        fn dataless(mut self, path: &str) -> Self {
            self.paths.insert(
                path.into(),
                Ok(InspectedPath {
                    kind: ExternalPathKind::File,
                    materialized: false,
                }),
            );
            self
        }

        fn unreadable(mut self, path: &str) -> Self {
            self = self.file(path);
            self.unreadable.push(path.into());
            self
        }
    }

    impl PathProbe for FakeProbe {
        fn inspect(&self, path: &Path) -> Result<InspectedPath, ExternalPathRejection> {
            self.paths
                .get(path)
                .cloned()
                .unwrap_or(Err(ExternalPathRejection::Missing))
        }

        fn readable(&self, path: &Path, _kind: ExternalPathKind) -> bool {
            !self.unreadable.iter().any(|candidate| candidate == path)
        }
    }

    fn paths(values: &[&str]) -> Vec<PathBuf> {
        values.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn empty_space_and_project_targets_choose_the_first_directory() {
        let probe = FakeProbe::default()
            .file("/tmp/notes.md")
            .directory("/tmp/first")
            .directory("/tmp/second");
        for target in [
            ExternalDropTarget::EmptySpace,
            ExternalDropTarget::Project { remote: false },
        ] {
            let plan = plan_external_drop_with(
                &paths(&["/tmp/notes.md", "/tmp/first", "/tmp/second"]),
                target,
                &probe,
            );
            assert_eq!(
                plan.action,
                Some(ExternalDropAction::OpenLauncher {
                    root: "/tmp/first".into()
                })
            );
            assert_eq!(plan.rejected.len(), 2);
            assert_eq!(
                plan.rejected[0].reason,
                ExternalPathRejection::RequiresDirectory
            );
            assert_eq!(
                plan.rejected[1].reason,
                ExternalPathRejection::AdditionalPath
            );
            assert!(plan.feedback().unwrap().starts_with("Ignored:"));
        }
    }

    #[test]
    fn session_target_attaches_every_valid_path_and_keeps_input_order() {
        let probe = FakeProbe::default()
            .directory("/tmp/a folder")
            .file("/tmp/$HOME; rm -rf nope");
        let plan = plan_external_drop_with(
            &paths(&["/tmp/a folder", "/tmp/$HOME; rm -rf nope"]),
            ExternalDropTarget::Session {
                id: SessionId("session-1".into()),
                remote: false,
            },
            &probe,
        );
        assert_eq!(
            plan.action,
            Some(ExternalDropAction::OpenSessionComposer {
                session_id: SessionId("session-1".into()),
                insertion: "'/tmp/a folder' '/tmp/$HOME; rm -rf nope'".into(),
            })
        );
        assert!(plan.rejected.is_empty());
    }

    #[test]
    fn invalid_unreadable_and_unmaterialized_paths_are_visible_rejections() {
        let probe = FakeProbe::default()
            .unreadable("/tmp/private")
            .dataless("/tmp/cloud.txt");
        let plan = plan_external_drop_with(
            &paths(&["/tmp/missing", "/tmp/private", "/tmp/cloud.txt"]),
            ExternalDropTarget::Session {
                id: SessionId("session-1".into()),
                remote: false,
            },
            &probe,
        );
        assert!(plan.action.is_none());
        assert_eq!(
            plan.rejected
                .iter()
                .map(|rejection| &rejection.reason)
                .collect::<Vec<_>>(),
            vec![
                &ExternalPathRejection::Missing,
                &ExternalPathRejection::Unreadable,
                &ExternalPathRejection::NotMaterialized,
            ]
        );
        let feedback = plan.feedback().unwrap();
        assert!(feedback.contains("does not exist"));
        assert!(feedback.contains("not readable"));
        assert!(feedback.contains("not downloaded"));
    }

    #[test]
    fn a_local_drop_on_a_remote_session_is_refused_before_filesystem_access() {
        let plan = plan_external_drop_with(
            &paths(&["/path/that/does/not/need/to/exist"]),
            ExternalDropTarget::Session {
                id: SessionId("remote".into()),
                remote: true,
            },
            &FakeProbe::default(),
        );
        assert!(plan.action.is_none());
        assert_eq!(plan.rejected[0].reason, ExternalPathRejection::RemoteTarget);
        assert!(plan.feedback().unwrap().contains("another host"));
    }

    #[test]
    fn a_terminal_drop_pastes_escaped_paths_in_order_with_trailing_spaces() {
        let probe = FakeProbe::default()
            .file("/tmp/Screen Shot 2026.png")
            .directory("/tmp/a folder")
            .dataless("/tmp/cloud.png");
        let plan = plan_terminal_drop_with(
            &paths(&[
                "/tmp/Screen Shot 2026.png",
                "/tmp/a folder",
                "/tmp/cloud.png",
            ]),
            false,
            &probe,
        );
        assert_eq!(
            plan.action,
            Some(TerminalDropAction::Paste(
                "/tmp/Screen\\ Shot\\ 2026.png /tmp/a\\ folder ".into()
            ))
        );
        assert_eq!(plan.rejected.len(), 1);
        assert!(plan.feedback().unwrap().starts_with("Ignored:"));
    }

    #[test]
    fn a_terminal_drop_on_a_remote_session_uploads_files_and_refuses_folders() {
        let probe = FakeProbe::default()
            .file("/tmp/shot.png")
            .directory("/tmp/a folder");
        let plan =
            plan_terminal_drop_with(&paths(&["/tmp/shot.png", "/tmp/a folder"]), true, &probe);
        assert_eq!(
            plan.action,
            Some(TerminalDropAction::Upload(vec![PathBuf::from(
                "/tmp/shot.png"
            )]))
        );
        assert_eq!(
            plan.rejected[0].reason,
            ExternalPathRejection::RemoteDirectory
        );

        let nothing = plan_terminal_drop_with(&paths(&["/tmp/missing"]), true, &probe);
        assert!(nothing.action.is_none());
        assert!(nothing.feedback().unwrap().starts_with("Couldn't use:"));
    }

    #[test]
    fn terminal_escaping_matches_what_macos_terminals_paste_on_drop() {
        assert_eq!(
            escape_path_for_terminal("/tmp/a b/$HOME;$(touch nope)'\"`x"),
            "/tmp/a\\ b/\\$HOME\\;\\$\\(touch\\ nope\\)\\'\\\"\\`x"
        );
        assert_eq!(
            escape_path_for_terminal("/Users/giga/Desktop/Ștampilă-2026_v1.png"),
            "/Users/giga/Desktop/Ștampilă-2026_v1.png"
        );
        assert_eq!(terminal_drop_text(["/a", "/b c"]), "/a /b\\ c ");
    }

    #[test]
    fn quoting_neutralizes_spaces_quotes_and_shell_metacharacters() {
        assert_eq!(
            quote_path("/tmp/a b/$HOME;$(touch nope)"),
            "'/tmp/a b/$HOME;$(touch nope)'"
        );
        assert_eq!(quote_path("/tmp/it's here"), "'/tmp/it'\\''s here'");
    }
}
