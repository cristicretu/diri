use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::Path;
use std::process::Command;

use tempfile::NamedTempFile;

const SCP: &str = "scp";
const REMOTE_TEMP_DIRECTORY: &str = "/tmp";

/// A clipboard image staged locally until `scp` has finished reading it.
pub(crate) struct StagedClipboardImage {
    local_file: NamedTempFile,
}

impl StagedClipboardImage {
    pub(crate) fn stage(bytes: &[u8], extension: &str) -> io::Result<Self> {
        let mut local_file = tempfile::Builder::new()
            .prefix("dirijor-clipboard-")
            .suffix(&format!(".{extension}"))
            .tempfile()?;
        local_file.write_all(bytes)?;
        local_file.flush()?;

        Ok(Self { local_file })
    }

    pub(crate) fn path(&self) -> &Path {
        self.local_file.path()
    }

    /// Uploads without invoking a shell, then returns the path that is valid
    /// inside the remote session. Dropping `self` removes the local staging
    /// file after scp exits.
    pub(crate) fn upload(self, ssh: &str) -> Result<String, String> {
        let file_name = self
            .local_file
            .path()
            .file_name()
            .ok_or_else(|| "clipboard image has no file name".to_owned())?
            .to_string_lossy()
            .into_owned();
        upload_to_remote_temp(self.local_file.path(), ssh, &file_name)
    }
}

/// Copies a file dropped on a remote session into that host's temp directory
/// and returns the path valid there. The remote name keeps the original file
/// name (so an Agent still sees `shot.png`, not an opaque id) under a unique
/// prefix, reduced to characters that are inert in the remote shell scp uses.
pub(crate) fn upload_dropped_file(local_path: &Path, ssh: &str) -> Result<String, String> {
    let file_name = local_path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", local_path.display()))?
        .to_string_lossy();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let remote_name = format!("dirijor-drop-{unique}-{}", remote_safe_name(&file_name));
    upload_to_remote_temp(local_path, ssh, &remote_name)
}

fn remote_safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    safe.trim_start_matches(['.', '-']).to_owned()
}

fn upload_to_remote_temp(
    local_path: &Path,
    ssh: &str,
    remote_name: &str,
) -> Result<String, String> {
    let remote_path = format!("{REMOTE_TEMP_DIRECTORY}/{remote_name}");
    let output = Command::new(SCP)
        .args(scp_arguments(local_path, ssh, &remote_path))
        .output()
        .map_err(|error| format!("could not start scp: {error}"))?;

    if output.status.success() {
        return Ok(remote_path);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        format!("scp exited with {}", output.status)
    } else {
        format!("scp failed: {detail}")
    })
}

fn scp_arguments(local_path: &Path, ssh: &str, remote_path: &str) -> Vec<OsString> {
    vec![
        "-q".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "--".into(),
        local_path.as_os_str().to_owned(),
        remote_destination(ssh, remote_path).into(),
    ]
}

fn remote_destination(ssh: &str, remote_path: &str) -> String {
    format!("{ssh}:{remote_path}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn scp_arguments_keep_paths_out_of_a_shell() {
        let args = scp_arguments(
            Path::new("/tmp/local image.png"),
            "cristi@forge",
            "/tmp/dirijor-clipboard-a1.png",
        );

        assert_eq!(args[0], "-q");
        assert_eq!(args[1], "-o");
        assert_eq!(args[2], "ConnectTimeout=10");
        assert_eq!(args[3], "--");
        assert_eq!(args[4], PathBuf::from("/tmp/local image.png"));
        assert_eq!(args[5], "cristi@forge:/tmp/dirijor-clipboard-a1.png");
    }

    #[test]
    fn dropped_file_names_are_reduced_to_shell_inert_characters() {
        assert_eq!(
            remote_safe_name("Screen Shot 2026 (1).png"),
            "Screen_Shot_2026__1_.png"
        );
        assert_eq!(remote_safe_name("$(rm -rf ~)`x`;y"), "__rm_-rf____x__y");
        assert_eq!(remote_safe_name(".hidden"), "hidden");
    }

    #[test]
    fn staging_preserves_the_image_and_generates_a_remote_temp_path() {
        let image = StagedClipboardImage::stage(b"png bytes", "png").unwrap();

        assert_eq!(std::fs::read(image.path()).unwrap(), b"png bytes");
        assert!(image.path().to_string_lossy().ends_with(".png"));
    }
}
