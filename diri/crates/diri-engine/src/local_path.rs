//! Local desktop PATH normalization shared by discovery and Agent launches.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Keep the shell's choices first, preserve inherited tools, then fill gaps
/// left by desktop launchers or failed shell initialization. Do not scan
/// version-manager installs: their shell-selected version remains authoritative.
pub fn search_path(
    shell_path: Option<&str>,
    environment: impl IntoIterator<Item = (String, String)>,
) -> String {
    let environment = environment
        .into_iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "PATH" | "HOME" | "PNPM_HOME" | "XDG_DATA_HOME"
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut directories = Vec::new();
    for path in [shell_path, environment.get("PATH").map(String::as_str)]
        .into_iter()
        .flatten()
    {
        directories.extend(
            path.split(':')
                .filter(|entry| !entry.is_empty())
                .map(PathBuf::from),
        );
    }
    let absolute = |key: &str| {
        environment
            .get(key)
            .map(Path::new)
            .filter(|path| path.is_absolute())
    };
    // pnpm <= 10 puts shims in PNPM_HOME; pnpm 11 uses PNPM_HOME/bin.
    // Retain both so upgrades and older global installs continue to work.
    let mut pnpm_homes = Vec::new();
    pnpm_homes.extend(absolute("PNPM_HOME").map(Path::to_path_buf));
    pnpm_homes.extend(absolute("XDG_DATA_HOME").map(|path| path.join("pnpm")));
    if let Some(home) = absolute("HOME") {
        if cfg!(target_os = "macos") {
            pnpm_homes.push(home.join("Library/pnpm"));
        }
        pnpm_homes.push(home.join(".local/share/pnpm"));
    }
    for home in pnpm_homes {
        directories.push(home.join("bin"));
        directories.push(home);
    }
    if let Some(home) = absolute("HOME") {
        for relative in [
            ".local/bin",
            ".bun/bin",
            ".cargo/bin",
            ".local/share/mise/shims",
            ".volta/bin",
        ] {
            directories.push(home.join(relative));
        }
    }
    directories.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .map(PathBuf::from),
    );
    let mut seen = HashSet::new();
    directories
        .into_iter()
        .filter_map(|directory| directory.to_str().map(str::to_owned))
        // A colon in HOME/PNPM_HOME must not inject additional PATH entries.
        .filter(|directory| !directory.contains(':') && !directory.contains('\0'))
        .filter(|directory| seen.insert(directory.clone()))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pnpm_codex_is_discovered_with_a_desktop_path() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let pnpm = home.join(if cfg!(target_os = "macos") {
            "Library/pnpm"
        } else {
            ".local/share/pnpm"
        });
        std::fs::create_dir_all(&pnpm).unwrap();
        let codex = pnpm.join("codex");
        std::fs::write(&codex, "#!/bin/sh\nprintf 'codex fixture\\n'\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = search_path(
            Some("/usr/bin:/bin"),
            [("HOME".into(), home.to_string_lossy().into_owned())],
        );
        assert_eq!(
            crate::agent_catalog::resolve_on_path("codex", &path),
            Some(codex.to_string_lossy().into_owned()),
            "pnpm-installed Codex must appear in the Agent catalog"
        );
        // Exercise the first-party login-shell wrapper with a fixture shell
        // so this test never loads the developer's or CI host's profiles.
        let shell = home.join("fixture-shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\nif [ \"$#\" -eq 4 ]; then exec /bin/sh -c \"$4\"; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let descriptor = crate::agent::AgentDescriptor {
            binary: Some("codex".into()),
            return_to_login_shell: true,
            ..Default::default()
        };
        let spec = descriptor
            .spawn_spec(
                home,
                [
                    ("HOME".into(), home.to_string_lossy().into_owned()),
                    ("SHELL".into(), shell.to_string_lossy().into_owned()),
                    ("PATH".into(), "/usr/bin:/bin".into()),
                ],
                &[],
            )
            .unwrap();
        let output = std::process::Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .env_clear()
            .envs(spec.env)
            .current_dir(home)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("codex fixture"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn package_manager_shims_and_their_runtime_share_the_launch_path() {
        use crate::agent::AgentDescriptor;

        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let runtime_dir = home.join("runtime with spaces");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let node = runtime_dir.join("diri-fixture-node");
        std::fs::write(&node, "#!/bin/sh\nprintf 'runtime reached\\n'\n").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();

        for (relative, variable) in [
            ("custom pnpm", "PNPM_HOME"),
            ("custom pnpm/bin", "PNPM_HOME"),
            ("data/pnpm", "XDG_DATA_HOME"),
            ("data/pnpm/bin", "XDG_DATA_HOME"),
            (".local/share/pnpm", "HOME"),
            (".local/share/pnpm/bin", "HOME"),
            (".bun/bin", "HOME"),
            (".local/share/mise/shims", "HOME"),
            (".volta/bin", "HOME"),
        ] {
            let bin = home.join(relative);
            std::fs::create_dir_all(&bin).unwrap();
            let codex = bin.join("codex");
            // pnpm launchers may depend on another executable on PATH.
            std::fs::write(&codex, "#!/bin/sh\nexec diri-fixture-node \"$@\"\n").unwrap();
            std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
            let value = match variable {
                "PNPM_HOME" => home.join("custom pnpm"),
                "XDG_DATA_HOME" => home.join("data"),
                _ => home.to_path_buf(),
            };
            let environment = vec![
                (variable.into(), value.to_string_lossy().into_owned()),
                ("PATH".into(), runtime_dir.to_string_lossy().into_owned()),
            ];
            let path = search_path(None, environment.clone());
            assert_eq!(
                crate::agent_catalog::resolve_on_path("codex", &path),
                Some(codex.to_string_lossy().into_owned()),
                "catalog must find {relative}"
            );
            let descriptor = AgentDescriptor {
                binary: Some("codex".into()),
                ..Default::default()
            };
            let spec = descriptor.spawn_spec(home, environment, &[]).unwrap();
            assert_eq!(spec.argv[0], codex.to_string_lossy());
            let output = std::process::Command::new(&spec.argv[0])
                .args(&spec.argv[1..])
                .env_clear()
                .envs(spec.env)
                .output()
                .unwrap();
            assert!(output.status.success(), "launcher failed for {relative}");
            assert_eq!(output.stdout, b"runtime reached\n");
            std::fs::remove_file(codex).unwrap();
        }
    }

    #[test]
    fn shell_and_inherited_paths_keep_precedence_without_duplicates() {
        let path = search_path(
            Some("/chosen node:/usr/bin"),
            [
                ("PATH".into(), "/inherited:/usr/bin:/chosen node".into()),
                ("PNPM_HOME".into(), "/custom/pnpm".into()),
            ],
        );
        assert!(
            path.starts_with("/chosen node:/usr/bin:/inherited:/custom/pnpm/bin:/custom/pnpm:")
        );
        assert_eq!(
            path.split(':').filter(|entry| *entry == "/usr/bin").count(),
            1
        );
        assert_eq!(search_path(None, [("PATH".into(), path.clone())]), path);
    }

    #[test]
    fn invalid_homes_do_not_add_relative_or_injected_search_directories() {
        let path = search_path(
            None,
            [
                ("HOME".into(), "relative".into()),
                ("PNPM_HOME".into(), "/unsafe:/injected".into()),
                ("XDG_DATA_HOME".into(), "".into()),
                ("PATH".into(), ":/usr/bin::/bin:".into()),
            ],
        );
        assert!(!path.contains("relative"));
        assert!(!path.contains("unsafe"));
        assert!(!path.contains("injected"));
        assert!(path.split(':').all(|entry| entry.starts_with('/')));
    }

    #[test]
    fn discovery_rejects_non_executable_files_and_accepts_package_manager_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = root.to_string_lossy();
        std::fs::write(root.join("codex"), "not executable").unwrap();
        assert!(crate::agent_catalog::resolve_on_path("codex", &path).is_none());
        std::fs::create_dir(root.join("directory")).unwrap();
        assert!(crate::agent_catalog::resolve_on_path("directory", &path).is_none());
        std::os::unix::fs::symlink(root.join("missing"), root.join("broken")).unwrap();
        assert!(crate::agent_catalog::resolve_on_path("broken", &path).is_none());
        std::os::unix::fs::symlink("/bin/sh", root.join("linked")).unwrap();
        assert_eq!(
            crate::agent_catalog::resolve_on_path("linked", &path),
            Some(root.join("linked").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn remote_launch_uses_only_the_remote_path() {
        let descriptor = crate::agent::AgentDescriptor {
            binary: Some("codex".into()),
            ..Default::default()
        };
        let spec = descriptor
            .remote_spawn_spec(
                Path::new("/remote"),
                [
                    ("PATH".into(), "/remote/bin".into()),
                    ("HOME".into(), "/remote/home".into()),
                ],
                &[],
            )
            .unwrap();
        assert_eq!(
            spec.env.iter().find(|(key, _)| key == "PATH").unwrap().1,
            "/remote/bin"
        );
    }
}
