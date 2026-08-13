//! One staging and delivery pipeline for visual context, regardless of
//! whether bytes arrived from the pasteboard or paths arrived from Finder.
//!
//! User-owned files are never handed directly to an agent. Every accepted
//! image is copied into a private temporary file with a generated name. This
//! both gives clipboard images a path and removes filenames containing prompt
//! delimiters or control characters from the delivery surface.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use diri_proto::{AgentDescriptor, AgentKind, ImageInputSpec, ImageInputStrategy};
use tempfile::NamedTempFile;

pub(crate) const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
pub(crate) const MAX_IMAGE_COUNT: usize = 12;
pub(crate) const MAX_TOTAL_IMAGE_BYTES: u64 = 100 * 1024 * 1024;
const DELIVERED_IMAGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const STAGING_DIRECTORY_PREFIX: &str = "diri-image-staging-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageFormat {
    fn from_extension(path: &Path) -> Option<Self> {
        Self::from_extension_text(path.extension()?.to_str()?)
    }

    fn from_extension_text(extension: &str) -> Option<Self> {
        match extension
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "gif" => Some(Self::Gif),
            "webp" => Some(Self::WebP),
            _ => None,
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }

    fn matches_header(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(&[0xff, 0xd8, 0xff]),
            Self::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            Self::WebP => {
                bytes.starts_with(b"RIFF") && bytes.get(8..12).is_some_and(|tag| tag == b"WEBP")
            }
        }
    }
}

pub(crate) fn is_supported_image_path(path: &Path) -> bool {
    ImageFormat::from_extension(path).is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageRejection {
    Missing,
    NotAFile,
    UnsupportedType,
    Unreadable,
    TooLarge,
    TooMany,
    TotalTooLarge,
    InvalidContents,
}

impl ImageRejection {
    pub(crate) const fn explanation(&self) -> &'static str {
        match self {
            Self::Missing => "does not exist",
            Self::NotAFile => "is not a regular file",
            Self::UnsupportedType => "is not a PNG, JPEG, GIF, or WebP image",
            Self::Unreadable => "could not be read",
            Self::TooLarge => "is larger than 20 MB",
            Self::TooMany => "exceeds the 12-image attachment limit",
            Self::TotalTooLarge => "would exceed the 100 MB attachment limit",
            Self::InvalidContents => "does not contain the image data its extension declares",
        }
    }
}

#[derive(Debug)]
struct StagedFile {
    /// `None` after a successful turn makes the private copy survive process
    /// exit long enough for the agent to open it. Draft-only files retain
    /// NamedTempFile's immediate unlink-on-drop behavior.
    file: Mutex<Option<NamedTempFile>>,
    path: PathBuf,
}

/// A private, app-owned image. Clones share the cleanup lease; the file is
/// removed when the final draft/submission reference is dropped.
#[derive(Clone, Debug)]
pub(crate) struct PendingImage {
    staged: Arc<StagedFile>,
    display_name: String,
    bytes: u64,
}

impl PendingImage {
    #[cfg(test)]
    pub(crate) fn stage_path(path: &Path) -> Result<Self, ImageRejection> {
        Self::stage_path_with_budget(path, MAX_TOTAL_IMAGE_BYTES)
    }

    /// Stage one path while honoring the remaining draft-wide byte budget.
    /// The descriptor size is checked before any copy and the bounded copy
    /// still protects against a file that grows after fstat.
    pub(crate) fn stage_path_with_budget(
        path: &Path,
        remaining_bytes: u64,
    ) -> Result<Self, ImageRejection> {
        let source = open_source(path)?;
        let metadata = source.metadata().map_err(|_| ImageRejection::Unreadable)?;
        if !metadata.file_type().is_file() {
            return Err(ImageRejection::NotAFile);
        }
        let format = ImageFormat::from_extension(path).ok_or(ImageRejection::UnsupportedType)?;
        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(safe_label)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("Image.{}", format.extension()));
        Self::stage_open_file(source, format, display_name, remaining_bytes)
    }

    fn stage_open_file(
        mut source: File,
        format: ImageFormat,
        display_name: String,
        remaining_bytes: u64,
    ) -> Result<Self, ImageRejection> {
        let metadata = source.metadata().map_err(|_| ImageRejection::Unreadable)?;
        if !metadata.file_type().is_file() {
            return Err(ImageRejection::NotAFile);
        }
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(ImageRejection::TooLarge);
        }
        if metadata.len() > remaining_bytes {
            return Err(ImageRejection::TotalTooLarge);
        }
        // Reject obvious mismatches before allocating/copying, then validate
        // the staged bytes again below so an in-place writer cannot win the
        // interval between this read and the bounded copy.
        validate_header(&mut source, format)?;
        source.rewind().map_err(|_| ImageRejection::Unreadable)?;
        let mut staged = private_temp(format).map_err(|_| ImageRejection::Unreadable)?;
        let copy_limit = MAX_IMAGE_BYTES.min(remaining_bytes);
        let copied = io::copy(&mut source.take(copy_limit + 1), staged.as_file_mut())
            .map_err(|_| ImageRejection::Unreadable)?;
        if copied > MAX_IMAGE_BYTES {
            return Err(ImageRejection::TooLarge);
        }
        if copied > remaining_bytes {
            return Err(ImageRejection::TotalTooLarge);
        }
        staged
            .as_file_mut()
            .flush()
            .map_err(|_| ImageRejection::Unreadable)?;
        staged
            .as_file_mut()
            .rewind()
            .map_err(|_| ImageRejection::Unreadable)?;
        validate_header(staged.as_file_mut(), format)?;
        Ok(Self {
            staged: Arc::new(staged_file(staged)),
            display_name,
            bytes: copied,
        })
    }

    pub(crate) fn stage_bytes(
        bytes: &[u8],
        extension: &str,
        display_name: &str,
    ) -> Result<Self, ImageRejection> {
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(ImageRejection::TooLarge);
        }
        let format =
            ImageFormat::from_extension_text(extension).ok_or(ImageRejection::UnsupportedType)?;
        if !format.matches_header(bytes) {
            return Err(ImageRejection::InvalidContents);
        }
        let mut staged = private_temp(format).map_err(|_| ImageRejection::Unreadable)?;
        staged
            .write_all(bytes)
            .and_then(|()| staged.flush())
            .map_err(|_| ImageRejection::Unreadable)?;
        Ok(Self {
            staged: Arc::new(staged_file(staged)),
            display_name: safe_label(display_name),
            bytes: bytes.len() as u64,
        })
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) const fn byte_len(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn local_path(&self) -> &Path {
        &self.staged.path
    }

    /// Keep the private copy alive after the daemon acknowledges submission.
    /// Persisting the tempfile before scheduling cleanup makes the lease
    /// survive an app exit; a later launch also prunes abandoned process
    /// directories once their owner is gone and the TTL has elapsed.
    pub(crate) fn cleanup_after_delivery(self, runtime: &tokio::runtime::Handle) {
        let path = self.staged.persist_for_delivery();
        match path {
            Some(path) => {
                drop(self);
                runtime.spawn(async move {
                    tokio::time::sleep(DELIVERED_IMAGE_TTL).await;
                    let _ = fs::remove_file(&path);
                    if let Some(directory) = path.parent() {
                        let _ = fs::remove_dir(directory);
                    }
                });
            }
            None => {
                // If persistence itself failed, retain NamedTempFile's open
                // lease for the same TTL. It will not survive app exit, but it
                // also will not be unlinked immediately after an acknowledged
                // submission.
                runtime.spawn(async move {
                    tokio::time::sleep(DELIVERED_IMAGE_TTL).await;
                    drop(self);
                });
            }
        }
    }
}

impl StagedFile {
    fn persist_for_delivery(&self) -> Option<PathBuf> {
        let mut file = self.file.lock().expect("staged image lock poisoned");
        let staged = file.take()?;
        match staged.keep() {
            Ok((_file, path)) => Some(path),
            Err(error) => {
                // `PersistError` still owns the tempfile, so restore it to the
                // shared lease and let the fallback TTL task retain it.
                *file = Some(error.file);
                None
            }
        }
    }
}

fn staged_file(file: NamedTempFile) -> StagedFile {
    StagedFile {
        path: file.path().to_path_buf(),
        file: Mutex::new(Some(file)),
    }
}

fn private_temp(format: ImageFormat) -> io::Result<NamedTempFile> {
    let directory = staging_directory()?;
    let staged = tempfile::Builder::new()
        .prefix("diri-image-")
        .suffix(&format!(".{}", format.extension()))
        .tempfile_in(directory)?;
    #[cfg(unix)]
    fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o600))?;
    Ok(staged)
}

fn staging_directory() -> io::Result<&'static Path> {
    static DIRECTORY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    match DIRECTORY.get_or_init(|| {
        let root = std::env::temp_dir().join("diri-image-attachments-v1");
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let metadata = fs::symlink_metadata(&root).map_err(|error| error.to_string())?;
        if !metadata.file_type().is_dir() {
            return Err("image staging root is not a directory".to_owned());
        }
        #[cfg(unix)]
        {
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err("image staging root is not owned by this user".to_owned());
            }
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
        }
        prune_stale_directories(&root);
        let directory = tempfile::Builder::new()
            .prefix(&format!(
                "{STAGING_DIRECTORY_PREFIX}{}-",
                std::process::id()
            ))
            .tempdir_in(&root)
            .map_err(|error| error.to_string())?
            .keep();
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        Ok(directory)
    }) {
        Ok(path) => Ok(path),
        Err(message) => Err(io::Error::other(message.clone())),
    }
}

/// Only directories created by this version are candidates, and a live owner
/// always wins over age. This avoids both startup-wide temp deletion and a
/// long-running Diri process losing an attachment another process still owns.
fn prune_stale_directories(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(remainder) = name.strip_prefix(STAGING_DIRECTORY_PREFIX) else {
            continue;
        };
        let Some(pid) = remainder.split('-').next().and_then(|pid| pid.parse().ok()) else {
            continue;
        };
        if process_is_alive(pid) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_dir() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= DELIVERED_IMAGE_TTL);
        if stale {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(pid: u32) -> bool {
    pid == std::process::id()
}

/// Open once, without following the final path component and without allowing
/// a swapped FIFO to block the UI worker. All later validation and copying use
/// this same descriptor, so a rename after this point cannot change the bytes
/// Diri stages.
fn open_source(path: &Path) -> Result<File, ImageRejection> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ImageRejection::Missing
        } else if error.raw_os_error() == Some(libc::ELOOP) {
            ImageRejection::NotAFile
        } else {
            ImageRejection::Unreadable
        }
    })
}

fn validate_header(file: &mut File, format: ImageFormat) -> Result<(), ImageRejection> {
    let mut header = [0_u8; 12];
    let read = file
        .read(&mut header)
        .map_err(|_| ImageRejection::Unreadable)?;
    format
        .matches_header(&header[..read])
        .then_some(())
        .ok_or(ImageRejection::InvalidContents)
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .take(120)
        .collect()
}

/// Cheap, metadata-only validation for drag highlighting. It deliberately does
/// not open the file, which could materialize an iCloud/File Provider object
/// merely because the pointer crossed a drop target. Staging repeats every
/// check and validates the signature after the explicit drop.
pub(crate) fn inspect_path_for_drag(path: &Path) -> Result<(), ImageRejection> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ImageRejection::Missing,
        _ => ImageRejection::Unreadable,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ImageRejection::NotAFile);
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(ImageRejection::TooLarge);
    }
    ImageFormat::from_extension(path)
        .map(|_| ())
        .ok_or(ImageRejection::UnsupportedType)
}

/// Full single-path inspection retained for focused validation tests. Product
/// drops stage directly so an explicit release cannot block the UI thread.
#[cfg(test)]
pub(crate) fn inspect_path(path: &Path) -> Result<(), ImageRejection> {
    let mut file = open_source(path)?;
    let metadata = file.metadata().map_err(|_| ImageRejection::Unreadable)?;
    if !metadata.file_type().is_file() {
        return Err(ImageRejection::NotAFile);
    }
    let format = ImageFormat::from_extension(path).ok_or(ImageRejection::UnsupportedType)?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(ImageRejection::TooLarge);
    }
    validate_header(&mut file, format)
}

pub(crate) fn can_add_image(
    existing_count: usize,
    existing_bytes: u64,
    bytes: u64,
) -> Result<(), ImageRejection> {
    if existing_count >= MAX_IMAGE_COUNT {
        return Err(ImageRejection::TooMany);
    }
    if existing_bytes.saturating_add(bytes) > MAX_TOTAL_IMAGE_BYTES {
        return Err(ImageRejection::TotalTooLarge);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageCapability {
    pub strategy: ImageInputStrategy,
    pub supports_image_only: bool,
    pub declared: bool,
}

pub(crate) fn capability(descriptor: Option<&AgentDescriptor>) -> ImageCapability {
    descriptor
        .and_then(|descriptor| descriptor.image_input.as_ref())
        .map_or(
            ImageCapability {
                strategy: ImageInputStrategy::PromptPath,
                supports_image_only: false,
                declared: false,
            },
            |spec: &ImageInputSpec| ImageCapability {
                strategy: spec.strategy,
                supports_image_only: spec.supports_image_only,
                declared: true,
            },
        )
}

/// Build one prompt without a shell or agent-specific keystroke. JSON string
/// literals make quotes, newlines, and metacharacters data rather than prompt
/// structure even if a future staging backend supplies such a path.
pub(crate) fn delivery_prompt(
    text: &str,
    paths: &[String],
    capability: ImageCapability,
) -> Result<String, &'static str> {
    let text = text.trim();
    if paths.is_empty() {
        return (!text.is_empty())
            .then(|| text.to_owned())
            .ok_or("Add instructions or an image");
    }
    if text.is_empty() && !capability.supports_image_only {
        return Err("This agent needs text with its image attachments");
    }
    match capability.strategy {
        ImageInputStrategy::PromptPath => {
            let mut prompt = text.to_owned();
            if !prompt.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str("Visual context (private staged image paths, in order):");
            for (index, path) in paths.iter().enumerate() {
                prompt.push_str(&format!(
                    "\n{}. {}",
                    index + 1,
                    serde_json::to_string(path).expect("strings always serialize")
                ));
            }
            Ok(prompt)
        }
    }
}

pub(crate) fn rejection_feedback(path: &Path, reason: &ImageRejection) -> String {
    format!(
        "Couldn't attach “{}”: {}.",
        path.to_string_lossy(),
        reason.explanation()
    )
}

/// Fail closed until a private, session-scoped uploader is available. Keeping
/// this decision at the delivery seam (rather than in a drop handler) means a
/// future uploader can replace it without weakening the no-auto-send rule.
pub(crate) fn delivery_blocker(
    kind: Option<&AgentKind>,
    remote: bool,
    attachment_count: usize,
) -> Option<&'static str> {
    if attachment_count == 0 {
        return None;
    }
    if kind.is_some_and(|kind| kind == &AgentKind::SHELL) {
        return Some("Image attachments require an agent session, not a plain Shell session");
    }
    remote.then_some("Remote image upload is unavailable in this build; nothing will be sent")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\nvalid enough for staging".to_vec()
    }

    fn declared(supports_image_only: bool) -> ImageCapability {
        ImageCapability {
            strategy: ImageInputStrategy::PromptPath,
            supports_image_only,
            declared: true,
        }
    }

    #[test]
    fn clipboard_and_file_bytes_use_the_same_private_staging_shape() {
        let clipboard = PendingImage::stage_bytes(&png(), "png", "Clipboard image").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("mockup.png");
        fs::write(&original, png()).unwrap();
        let dropped = PendingImage::stage_path(&original).unwrap();

        assert_ne!(clipboard.local_path(), dropped.local_path());
        assert_eq!(fs::read(clipboard.local_path()).unwrap(), png());
        assert_eq!(fs::read(dropped.local_path()).unwrap(), png());
        assert_eq!(dropped.display_name(), "mockup.png");
    }

    #[test]
    fn all_supported_raster_formats_are_accepted_case_insensitively() {
        for (extension, bytes) in [
            ("PNG", b"\x89PNG\r\n\x1a\nrest".as_slice()),
            ("jpeg", b"\xff\xd8\xffrest".as_slice()),
            ("gif", b"GIF89arest".as_slice()),
            ("WebP", b"RIFF\x04\0\0\0WEBPrest".as_slice()),
        ] {
            PendingImage::stage_bytes(bytes, extension, "image")
                .unwrap_or_else(|error| panic!("{extension}: {error:?}"));
        }
    }

    #[test]
    fn invalid_type_readability_size_and_contents_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let text = directory.path().join("notes.txt");
        fs::write(&text, b"not an image").unwrap();
        assert_eq!(inspect_path(&text), Err(ImageRejection::UnsupportedType));
        let fake = directory.path().join("fake.png");
        fs::write(&fake, b"not png").unwrap();
        assert_eq!(inspect_path(&fake), Err(ImageRejection::InvalidContents));
        assert_eq!(
            inspect_path(&directory.path().join("missing.png")),
            Err(ImageRejection::Missing)
        );
        assert_eq!(
            inspect_path(directory.path()),
            Err(ImageRejection::NotAFile)
        );
        assert_eq!(
            PendingImage::stage_bytes(&vec![0; MAX_IMAGE_BYTES as usize + 1], "png", "large")
                .unwrap_err(),
            ImageRejection::TooLarge
        );
    }

    #[test]
    fn multiple_images_keep_order_and_dangerous_paths_are_json_escaped() {
        let prompt = delivery_prompt(
            "compare",
            &[
                "/tmp/first image.png".into(),
                "/tmp/'\n$(touch nope).jpg".into(),
            ],
            declared(true),
        )
        .unwrap();
        assert_eq!(
            prompt,
            "compare\n\nVisual context (private staged image paths, in order):\n1. \"/tmp/first image.png\"\n2. \"/tmp/'\\n$(touch nope).jpg\""
        );
        assert!(!prompt.contains("\n$(touch"));
    }

    #[test]
    fn declared_agents_allow_image_only_but_manifest_fallback_requires_text() {
        let fallback = capability(None);
        assert!(!fallback.declared);
        assert_eq!(
            delivery_prompt("", &["/tmp/a.png".into()], fallback),
            Err("This agent needs text with its image attachments")
        );
        assert!(delivery_prompt("", &["/tmp/a.png".into()], declared(true)).is_ok());
    }

    #[test]
    fn typed_client_descriptor_exposes_claude_and_codex_manifest_capabilities() {
        for id in ["claude-code", "codex"] {
            let descriptor = AgentDescriptor {
                id: id.to_owned(),
                display_name: id.to_owned(),
                image_input: Some(ImageInputSpec {
                    strategy: ImageInputStrategy::PromptPath,
                    supports_image_only: true,
                }),
                ..AgentDescriptor::default()
            };
            let capability = capability(Some(&descriptor));
            assert!(capability.declared, "{id}");
            assert!(capability.supports_image_only, "{id}");
            assert_eq!(capability.strategy, ImageInputStrategy::PromptPath);
        }
    }

    #[test]
    fn staging_lease_cleans_the_copy_without_touching_the_original() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.png");
        fs::write(&original, png()).unwrap();
        let staged = PendingImage::stage_path(&original).unwrap();
        let private = staged.local_path().to_path_buf();
        drop(staged);
        assert!(original.exists());
        assert!(!private.exists());
    }

    #[test]
    fn delivered_lease_survives_drop_until_explicit_cleanup() {
        let staged = PendingImage::stage_bytes(&png(), "png", "delivered.png").unwrap();
        let private = staged.local_path().to_path_buf();
        assert_eq!(staged.staged.persist_for_delivery(), Some(private.clone()));
        drop(staged);
        assert!(private.exists());
        fs::remove_file(private).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn staging_files_and_directories_are_owner_only() {
        let staged = PendingImage::stage_bytes(&png(), "png", "private.png").unwrap();
        assert_eq!(
            fs::metadata(staged.local_path()).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(staged.local_path().parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_fifos_are_rejected_without_following_or_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.png");
        fs::write(&original, png()).unwrap();
        let link = directory.path().join("link.png");
        symlink(&original, &link).unwrap();
        assert_eq!(
            PendingImage::stage_path(&link).unwrap_err(),
            ImageRejection::NotAFile
        );

        let fifo = directory.path().join("pipe.png");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert_eq!(
            PendingImage::stage_path(&fifo).unwrap_err(),
            ImageRejection::NotAFile
        );
    }

    #[test]
    fn validation_and_copy_use_the_descriptor_that_was_opened_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("race.png");
        let moved = directory.path().join("original.png");
        let original = b"\x89PNG\r\n\x1a\noriginal bytes";
        let replacement = b"\x89PNG\r\n\x1a\nreplacement bytes";
        fs::write(&path, original).unwrap();
        let source = open_source(&path).unwrap();
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, replacement).unwrap();

        let staged = PendingImage::stage_open_file(
            source,
            ImageFormat::Png,
            "race.png".to_owned(),
            MAX_TOTAL_IMAGE_BYTES,
        )
        .unwrap();
        assert_eq!(fs::read(staged.local_path()).unwrap(), original);
        assert_eq!(staged.byte_len(), original.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn stale_cleanup_only_prunes_owned_shape_directories_after_the_ttl() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let root = tempfile::tempdir().unwrap();
        let stale = root
            .path()
            .join(format!("{STAGING_DIRECTORY_PREFIX}99999999-stale"));
        let unrelated = root.path().join("another-app-99999999-stale");
        fs::create_dir(&stale).unwrap();
        fs::create_dir(&unrelated).unwrap();
        let stale_c = CString::new(stale.as_os_str().as_bytes()).unwrap();
        let old = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe {
                libc::utimensat(
                    libc::AT_FDCWD,
                    stale_c.as_ptr(),
                    [old, old].as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            },
            0
        );

        prune_stale_directories(root.path());

        assert!(!stale.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn count_and_aggregate_limits_fail_closed() {
        assert_eq!(
            can_add_image(MAX_IMAGE_COUNT, 0, 1),
            Err(ImageRejection::TooMany)
        );
        assert_eq!(
            can_add_image(0, MAX_TOTAL_IMAGE_BYTES, 1),
            Err(ImageRejection::TotalTooLarge)
        );
        assert_eq!(
            can_add_image(MAX_IMAGE_COUNT - 1, MAX_TOTAL_IMAGE_BYTES - 1, 1),
            Ok(())
        );

        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("budget.png");
        fs::write(&image, png()).unwrap();
        assert_eq!(
            PendingImage::stage_path_with_budget(&image, 4).unwrap_err(),
            ImageRejection::TotalTooLarge
        );
    }

    #[test]
    fn shell_and_remote_delivery_fail_closed_before_a_turn_can_be_built() {
        assert_eq!(delivery_blocker(Some(&AgentKind::CODEX), false, 2), None);
        assert_eq!(delivery_blocker(Some(&AgentKind::CODEX), true, 0), None);
        assert_eq!(
            delivery_blocker(Some(&AgentKind::CODEX), true, 2),
            Some("Remote image upload is unavailable in this build; nothing will be sent")
        );
        assert_eq!(
            delivery_blocker(Some(&AgentKind::SHELL), false, 1),
            Some("Image attachments require an agent session, not a plain Shell session")
        );
        assert_eq!(
            delivery_blocker(Some(&AgentKind::new("unknown")), false, 1),
            None
        );
    }
}
