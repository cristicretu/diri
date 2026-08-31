//! Bounded executable discovery for one remote account.
//!
//! One login-environment capture serves the whole manifest catalog. Resolution
//! is direct filesystem metadata work; no `which` subprocess or shell string
//! is created per Agent.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use diri_proto::remote_pty::{
    EnvironmentCaptureRequest, ExecutableDiscoveryItem, ExecutableDiscoveryRequest,
    ExecutableDiscoveryResult,
};

pub(crate) fn discover(
    request: &ExecutableDiscoveryRequest,
    executable: &Path,
) -> io::Result<ExecutableDiscoveryResult> {
    request
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let environment = crate::environment::capture(
        &EnvironmentCaptureRequest {
            cwd: request.cwd.clone(),
            timeout_millis: request.timeout_millis,
        },
        executable,
    )?;
    let path = environment
        .environment
        .iter()
        .rev()
        .find(|variable| variable.name == "PATH")
        .map(|variable| variable.value.as_str())
        .unwrap_or("");

    let mut items = Vec::with_capacity(request.queries.len());
    for query in &request.queries {
        let detected_path = resolve_on_path(&query.binary, path);
        let (configured_path, configured_error) = match query.configured_path.as_deref() {
            Some(configured) => match validate_configured_path(configured) {
                Ok(path) => (Some(path), None),
                Err(error) => (None, Some(error.to_string())),
            },
            None => (None, None),
        };
        items.push(ExecutableDiscoveryItem {
            id: query.id.clone(),
            detected_path,
            configured_path,
            configured_error,
        });
    }
    Ok(ExecutableDiscoveryResult { environment, items })
}

fn resolve_on_path(binary: &str, path: &str) -> Option<String> {
    if binary.contains('/') {
        return validate_configured_path(binary).ok();
    }
    path.split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(binary))
        .find_map(executable_path)
}

fn validate_configured_path(path: &str) -> io::Result<String> {
    let expanded = expand_home(path)?;
    if !expanded.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path must be absolute or home-relative",
        ));
    }
    executable_path(expanded).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not an executable regular file",
        )
    })
}

fn executable_path(path: impl Into<PathBuf>) -> Option<String> {
    let path = path.into();
    let metadata = fs::metadata(&path).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then(|| path.to_string_lossy().into_owned())
}

fn expand_home(path: &str) -> io::Result<PathBuf> {
    if path == "~" || path.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let mut expanded = PathBuf::from(home);
        if let Some(rest) = path.strip_prefix("~/") {
            expanded.push(rest);
        }
        Ok(expanded)
    } else {
        Ok(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_first_executable_and_ignores_non_executable() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        fs::write(first.path().join("codex"), b"no").expect("write");
        let executable = second.path().join("codex");
        fs::write(&executable, b"yes").expect("write");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("chmod");
        let path = format!("{}:{}", first.path().display(), second.path().display());
        assert_eq!(
            resolve_on_path("codex", &path).as_deref(),
            Some(executable.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn configured_path_must_be_absolute_and_executable() {
        assert!(validate_configured_path("relative/codex").is_err());
        let temp = tempfile::tempdir().expect("temp");
        let file = temp.path().join("codex");
        fs::write(&file, b"x").expect("write");
        assert!(validate_configured_path(&file.to_string_lossy()).is_err());
    }
}
