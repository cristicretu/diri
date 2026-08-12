//! One staging and delivery pipeline for visual context, regardless of
//! whether bytes arrived from the pasteboard or paths arrived from Finder.
//!
//! User-owned files are never handed directly to an agent. Every accepted
//! image is copied into a private temporary file with a generated name. This
//! both gives clipboard images a path and removes filenames containing prompt
//! delimiters or control characters from the delivery surface.

use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use diri_proto::{AgentDescriptor, AgentKind, ImageInputSpec, ImageInputStrategy};
use tempfile::NamedTempFile;

pub(crate) const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

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
            Self::InvalidContents => "does not contain the image data its extension declares",
        }
    }
}

#[derive(Debug)]
struct StagedFile {
    file: NamedTempFile,
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
    pub(crate) fn stage_path(path: &Path) -> Result<Self, ImageRejection> {
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
        let format = ImageFormat::from_extension(path).ok_or(ImageRejection::UnsupportedType)?;
        let mut source = File::open(path).map_err(|_| ImageRejection::Unreadable)?;
        validate_header(&mut source, format)?;
        source.rewind().map_err(|_| ImageRejection::Unreadable)?;
        let mut staged = private_temp(format).map_err(|_| ImageRejection::Unreadable)?;
        let copied = io::copy(&mut source.take(MAX_IMAGE_BYTES + 1), staged.as_file_mut())
            .map_err(|_| ImageRejection::Unreadable)?;
        if copied > MAX_IMAGE_BYTES {
            return Err(ImageRejection::TooLarge);
        }
        staged
            .as_file_mut()
            .flush()
            .map_err(|_| ImageRejection::Unreadable)?;
        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(safe_label)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("Image.{}", format.extension()));
        Ok(Self {
            staged: Arc::new(StagedFile { file: staged }),
            display_name,
            bytes: metadata.len(),
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
            staged: Arc::new(StagedFile { file: staged }),
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
        self.staged.file.path()
    }

    /// Keep the private copy alive briefly after submission. Queueing a PTY
    /// write is not the same instant as the agent opening the path; immediate
    /// unlinking makes fast cleanup race correct delivery.
    pub(crate) fn cleanup_after_delivery(self, runtime: &tokio::runtime::Handle) {
        runtime.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5 * 60)).await;
            drop(self);
        });
    }
}

fn private_temp(format: ImageFormat) -> io::Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix("diri-image-")
        .suffix(&format!(".{}", format.extension()))
        .tempfile()
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

/// Cheap validation for drag highlighting. Staging repeats the checks on drop
/// because Finder payloads are mutable and may disappear between the events.
pub(crate) fn inspect_path(path: &Path) -> Result<(), ImageRejection> {
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
    let format = ImageFormat::from_extension(path).ok_or(ImageRejection::UnsupportedType)?;
    let mut file = File::open(path).map_err(|_| ImageRejection::Unreadable)?;
    validate_header(&mut file, format)
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
