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
            .to_string_lossy();
        let remote_path = format!("{REMOTE_TEMP_DIRECTORY}/{file_name}");
        let output = Command::new(SCP)
            .args(scp_arguments(self.local_file.path(), ssh, &remote_path))
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
    fn staging_preserves_the_image_and_generates_a_remote_temp_path() {
        let image = StagedClipboardImage::stage(b"png bytes", "png").unwrap();

        assert_eq!(std::fs::read(image.path()).unwrap(), b"png bytes");
        assert!(image.path().to_string_lossy().ends_with(".png"));
    }
}
