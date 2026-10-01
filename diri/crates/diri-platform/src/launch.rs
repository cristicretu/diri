//! Executable discovery and interpreter selection without shell-command building.
use std::{
    io,
    path::{Path, PathBuf},
};

pub fn resolve_argv(
    argv: &[String],
    env: &[(String, String)],
    cwd: &Path,
) -> io::Result<Vec<String>> {
    let program = argv
        .first()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty executable"))?;
    #[cfg(not(windows))]
    {
        let _ = (program, env, cwd);
        Ok(argv.to_vec())
    }
    #[cfg(windows)]
    {
        let executable = find_executable(program, env, cwd)?;
        let extension = executable
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut result = match extension.as_str() {
            "exe" | "com" => vec![path_string(&executable)?],
            "ps1" => {
                let shell = find_executable("pwsh.exe", env, cwd)
                    .or_else(|_| find_executable("powershell.exe", env, cwd))?;
                vec![
                    path_string(&shell)?,
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-File".into(),
                    path_string(&executable)?,
                ]
            }
            "cmd" | "bat" => npm_shim(&executable, env, cwd)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Windows executable must be an exe, com, PowerShell script, or recognized npm shim",
                ));
            }
        };
        result.extend_from_slice(&argv[1..]);
        Ok(result)
    }
}

pub fn find_executable(program: &str, env: &[(String, String)], cwd: &Path) -> io::Result<PathBuf> {
    let supplied = Path::new(program);
    let explicit = supplied.is_absolute() || supplied.components().count() > 1;
    let roots: Vec<PathBuf> = if explicit {
        vec![cwd.to_path_buf()]
    } else {
        let path = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "launch PATH is unavailable"))?;
        std::env::split_paths(path)
            .filter(|p| p.is_absolute())
            .collect()
    };
    for root in roots {
        let base = root.join(supplied);
        #[cfg(windows)]
        let candidates = if base.extension().is_some() {
            vec![base]
        } else {
            // Native images win over interpreter shims. Never consult the
            // parent process environment or implicitly search the current dir.
            let mut extensions = vec![".exe".to_owned(), ".com".to_owned()];
            if let Some((_, value)) = env
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("PATHEXT"))
            {
                extensions.extend(
                    value
                        .split(';')
                        .filter(|v| {
                            matches!(v.to_ascii_lowercase().as_str(), ".cmd" | ".bat" | ".ps1")
                        })
                        .map(str::to_owned),
                );
            }
            extensions.extend([".cmd".into(), ".bat".into(), ".ps1".into()]);
            extensions
                .into_iter()
                .map(|ext| PathBuf::from(format!("{}{ext}", base.display())))
                .collect()
        };
        #[cfg(not(windows))]
        let candidates = vec![base];
        for candidate in candidates {
            if candidate.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if std::fs::metadata(&candidate)?.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                #[cfg(windows)]
                return crate::canonicalize(candidate);
                #[cfg(unix)]
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("executable not found: {program}"),
    ))
}

#[cfg(windows)]
fn path_string(path: &Path) -> io::Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path is not Unicode",
        )
    })
}

#[cfg(windows)]
fn npm_shim(path: &Path, env: &[(String, String)], cwd: &Path) -> io::Result<Vec<String>> {
    use std::io::Read;
    let mut source = String::new();
    std::fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_string(&mut source)?;
    if source.len() > 64 * 1024 {
        return Err(io::Error::other("oversized batch shim"));
    }
    // npm's generated cmd-shim names one JS entry under %dp0%/node_modules
    // (older versions use %~dp0). Read that literal path as data. User args
    // never enter cmd.exe; Node receives the original structured vector.
    let source_lower = source.to_ascii_lowercase();
    if !source_lower.contains("node") || !source.contains("%*") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "custom batch launch is unsupported; select the native executable or a PowerShell script",
        ));
    }
    let mut scripts = Vec::new();
    for quoted in source.split('"').skip(1).step_by(2) {
        let suffix = quoted
            .strip_prefix("%dp0%\\")
            .or_else(|| quoted.strip_prefix("%~dp0\\"))
            .or_else(|| quoted.strip_prefix("%~dp0"));
        let Some(suffix) = suffix else {
            continue;
        };
        if suffix.contains('%') || !suffix.starts_with("node_modules\\") {
            continue;
        }
        let script = path.parent().unwrap_or(cwd).join(suffix);
        if script.is_file()
            && matches!(
                script.extension().and_then(|e| e.to_str()),
                Some("js" | "cjs" | "mjs")
            )
        {
            let script = crate::canonicalize(script)?;
            if !scripts.contains(&script) {
                scripts.push(script);
            }
        }
    }
    if scripts.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "batch shim has no unambiguous npm JavaScript entry; select the native executable",
        ));
    }
    let sibling = path.with_file_name("node.exe");
    let node = if sibling.is_file() {
        sibling
    } else {
        find_executable("node.exe", env, cwd)?
    };
    Ok(vec![path_string(&node)?, path_string(&scripts[0])?])
}

/// Whether a directory-picker entry can name a supported native launch target.
pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
            matches!(
                s.to_ascii_lowercase().as_str(),
                "exe" | "com" | "cmd" | "bat" | "ps1"
            )
        })
    }
}

#[cfg(windows)]
pub fn default_shell() -> String {
    let environment = std::env::vars().collect::<Vec<_>>();
    let cwd = std::env::current_dir().unwrap_or_default();
    find_executable("pwsh.exe", &environment, &cwd)
        .or_else(|_| find_executable("powershell.exe", &environment, &cwd))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "powershell.exe".into())
}

/// Native children receive an explicit environment; remote children continue
/// to use the Helper's host-side capture. Windows names are case insensitive.
pub fn local_environment() -> Vec<(String, String)> {
    #[cfg(unix)]
    {
        std::env::vars().collect()
    }
    #[cfg(windows)]
    {
        let mut values = std::collections::BTreeMap::new();
        for (key, value) in std::env::vars() {
            let name = key.to_ascii_uppercase();
            let standard = matches!(
                name.as_str(),
                "SYSTEMROOT"
                    | "WINDIR"
                    | "COMSPEC"
                    | "PATH"
                    | "PATHEXT"
                    | "USERPROFILE"
                    | "HOMEDRIVE"
                    | "HOMEPATH"
                    | "HOME"
                    | "USERNAME"
                    | "USERDOMAIN"
                    | "APPDATA"
                    | "LOCALAPPDATA"
                    | "PROGRAMDATA"
                    | "PROGRAMFILES"
                    | "PROGRAMFILES(X86)"
                    | "PROGRAMW6432"
                    | "TEMP"
                    | "TMP"
                    | "SHELL"
                    | "LANG"
                    | "LC_ALL"
                    | "TZ"
                    | "HTTP_PROXY"
                    | "HTTPS_PROXY"
                    | "ALL_PROXY"
                    | "NO_PROXY"
                    | "SSH_AUTH_SOCK"
                    | "GIT_SSH"
                    | "GIT_SSH_COMMAND"
                    | "GIT_CONFIG_GLOBAL"
                    | "GIT_CONFIG_NOSYSTEM"
                    | "EDITOR"
                    | "VISUAL"
                    | "NODE_EXTRA_CA_CERTS"
                    | "SSL_CERT_FILE"
                    | "SSL_CERT_DIR"
                    | "CARGO_HOME"
                    | "RUSTUP_HOME"
                    | "BUN_INSTALL"
                    | "PNPM_HOME"
                    | "VIRTUAL_ENV"
                    | "PYTHONPATH"
                    | "CODEX_HOME"
                    | "CLAUDE_CONFIG_DIR"
                    | "CLAUDE_CODE_GIT_BASH_PATH"
            );
            let agent = [
                "ANTHROPIC_",
                "OPENAI_",
                "GEMINI_",
                "GOOGLE_",
                "AZURE_OPENAI_",
                "AWS_",
                "CLAUDE_CODE_",
                "CODEX_",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix));
            if standard || agent {
                values.insert(name, value);
            }
        }
        if !values.contains_key("HOME")
            && let Some(home) = values.get("USERPROFILE").cloned()
        {
            values.insert("HOME".into(), home);
        }
        values.into_iter().collect()
    }
}

/// Only fixed internal POSIX maintenance scripts use Git for Windows' shell.
/// Agent launches always use resolve_argv and never this interpreter.
/// PATH for a [`maintenance_shell`] child. MSYS mounts the Git root at `/`,
/// so a user PATH naming `<Git>\bin` becomes `/usr/bin`, which has no `git`.
/// `<Git>\cmd` holds the `git` launcher in every Git for Windows layout.
#[cfg(windows)]
pub fn maintenance_path(shell: &Path) -> Option<std::ffi::OsString> {
    // <Git>\usr\bin\sh.exe -> <Git>
    let command = shell.parent()?.parent()?.parent()?.join("cmd");
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(command).chain(std::env::split_paths(&inherited))).ok()
}

pub fn maintenance_shell() -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        Ok("/bin/sh".into())
    }
    #[cfg(windows)]
    {
        let environment = local_environment();
        let git = find_executable("git.exe", &environment, &std::env::current_dir()?)?;
        for ancestor in git.ancestors().skip(1).take(4) {
            let shell = ancestor.join("usr/bin/sh.exe");
            if shell.is_file() {
                return Ok(shell);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Git for Windows is required for repository migration maintenance",
        ))
    }
}
