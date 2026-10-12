//! What diri runs when macOS hands it a link or a file as the default
//! terminal: `ssh://` and `telnet://` links, `x-man-page://` links, and
//! `.command` / `.tool` / `.sh` files.
//!
//! Everything a link carries is untrusted: a web page can put any `ssh://`
//! link in front of the user. Each request therefore becomes one structured
//! argv, exec'd directly in a new terminal, and never a shell command line.
//! Every value from the link is checked against a narrow character set, so a
//! host or user that would read as an option (`-oProxyCommand=…`) or carry
//! `ssh_config` tokens (`%h`) is refused before anything runs.

use std::path::{Path, PathBuf};

use url::{Host, Url};

use crate::i18n::{t, tf};

/// The system OpenSSH client. A link names a host, never a program.
const SSH: &str = "/usr/bin/ssh";
const MAN: &str = "/usr/bin/man";
/// macOS stopped shipping telnet; Homebrew's is used where it is installed.
const TELNET_CANDIDATES: [&str; 3] = [
    "/opt/homebrew/bin/telnet",
    "/usr/local/bin/telnet",
    "/usr/bin/telnet",
];
/// Runs the opened file (`$0`), says that it ended, and leaves the user in
/// their login shell in its folder, as Terminal does, so the output stays
/// readable. The text is fixed: the file's path reaches it only as `$0`.
const RUN_THEN_SHELL: &str = r#""$0"; status=$?; printf '\n[Process completed with exit code %s]\n' "$status"; exec "${SHELL:-/bin/zsh}" -l"#;

/// Links longer than this are not something a person clicked on.
const MAX_LINK_BYTES: usize = 4096;
const MAX_HOST_BYTES: usize = 253;
const MAX_USER_BYTES: usize = 256;
const MAX_PAGE_BYTES: usize = 128;

/// Document types diri opens (declared in `assets/Info.plist`).
const RUNNABLE_EXTENSIONS: [&str; 3] = ["command", "tool", "sh"];

/// A new local terminal: what it runs, where, and what its tab says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenRequest {
    pub(crate) argv: Vec<String>,
    /// The opened file's folder; `None` starts where a new terminal would.
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenError {
    /// Not a link or file type diri handles.
    Unsupported,
    TooLong,
    InvalidHost,
    InvalidUser,
    InvalidPort,
    InvalidPage,
    NotFound(String),
    NotExecutable(String),
    /// Downloaded from the internet and not yet opened deliberately.
    Quarantined(String),
    TelnetMissing,
}

impl OpenError {
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Unsupported => t("open_handler.error.unsupported").to_owned(),
            Self::TooLong => t("open_handler.error.too_long").to_owned(),
            Self::InvalidHost => t("open_handler.error.host").to_owned(),
            Self::InvalidUser => t("open_handler.error.user").to_owned(),
            Self::InvalidPort => t("open_handler.error.port").to_owned(),
            Self::InvalidPage => t("open_handler.error.page").to_owned(),
            Self::NotFound(name) => tf("open_handler.error.not_found", &[("name", name)]),
            Self::NotExecutable(name) => tf("open_handler.error.not_executable", &[("name", name)]),
            Self::Quarantined(name) => tf("open_handler.error.quarantined", &[("name", name)]),
            Self::TelnetMissing => t("open_handler.error.telnet").to_owned(),
        }
    }
}

/// What a request needs to know about the machine, behind a seam so tests
/// never depend on real files or installed programs.
pub(crate) trait SystemFacts {
    fn file(&self, path: &Path) -> Option<FileFacts>;
    fn is_executable_program(&self, path: &Path) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileFacts {
    pub(crate) regular: bool,
    pub(crate) executable: bool,
    pub(crate) quarantined: bool,
}

/// Turns one link macOS delivered into the terminal to open.
pub(crate) fn request_for(link: &str, system: &impl SystemFacts) -> Result<OpenRequest, OpenError> {
    if link.len() > MAX_LINK_BYTES {
        return Err(OpenError::TooLong);
    }
    let scheme = link
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .ok_or(OpenError::Unsupported)?;
    match scheme.as_str() {
        "ssh" => ssh(link),
        "telnet" => telnet(link, system),
        "x-man-page" => man_page(&link[scheme.len() + 1..]),
        "file" => file(link, system),
        _ => Err(OpenError::Unsupported),
    }
}

/// The real machine.
pub(crate) struct LocalSystem;

impl SystemFacts for LocalSystem {
    fn file(&self, path: &Path) -> Option<FileFacts> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(FileFacts {
            regular: metadata.is_file(),
            executable: is_executable(path),
            quarantined: is_quarantined(path),
        })
    }

    fn is_executable_program(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) && is_executable(path)
    }
}

#[cfg(unix)]
fn c_path(path: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).ok()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    // SAFETY: `access` reads a NUL-terminated path and touches nothing else.
    c_path(path).is_some_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn is_quarantined(path: &Path) -> bool {
    let Some(path) = c_path(path) else {
        return true;
    };
    // SAFETY: a null buffer with size 0 only asks for the attribute's length.
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    length >= 0
}

#[cfg(not(target_os = "macos"))]
fn is_quarantined(_: &Path) -> bool {
    false
}

struct Endpoint {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

/// `scheme://[user[;params]][:password]@host[:port][/]`. A password is never
/// passed on; the program asks for it.
fn endpoint(link: &str) -> Result<Endpoint, OpenError> {
    let url = Url::parse(link).map_err(|error| match error {
        url::ParseError::InvalidPort => OpenError::InvalidPort,
        _ => OpenError::InvalidHost,
    })?;
    let host = match url.host() {
        Some(Host::Domain(domain)) => {
            let host = percent_decode(domain).ok_or(OpenError::InvalidHost)?;
            if !valid_host(&host) {
                return Err(OpenError::InvalidHost);
            }
            host
        }
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => address.to_string(),
        None => return Err(OpenError::InvalidHost),
    };
    let user = match url.username() {
        "" => None,
        encoded => {
            let decoded = percent_decode(encoded).ok_or(OpenError::InvalidUser)?;
            // The SSH URI draft's `;fingerprint=…` connection parameters
            // (which the parser hands back percent-encoded).
            let user = decoded.split(';').next().unwrap_or_default().to_owned();
            if !valid_user(&user) {
                return Err(OpenError::InvalidUser);
            }
            Some(user)
        }
    };
    let port = match url.port() {
        Some(0) => return Err(OpenError::InvalidPort),
        port => port,
    };
    Ok(Endpoint { user, host, port })
}

fn ssh(link: &str) -> Result<OpenRequest, OpenError> {
    let Endpoint { user, host, port } = endpoint(link)?;
    let mut argv = vec![SSH.to_owned()];
    if let Some(user) = &user {
        argv.extend(["-l".to_owned(), user.clone()]);
    }
    if let Some(port) = port {
        argv.extend(["-p".to_owned(), port.to_string()]);
    }
    // Belt and braces: the host is already known not to start with `-`.
    argv.extend(["--".to_owned(), host.clone()]);
    let title = match &user {
        Some(user) => format!("ssh {user}@{host}"),
        None => format!("ssh {host}"),
    };
    Ok(OpenRequest {
        argv,
        cwd: None,
        title,
    })
}

fn telnet(link: &str, system: &impl SystemFacts) -> Result<OpenRequest, OpenError> {
    let Endpoint { user, host, port } = endpoint(link)?;
    let program = TELNET_CANDIDATES
        .iter()
        .find(|candidate| system.is_executable_program(Path::new(candidate)))
        .ok_or(OpenError::TelnetMissing)?;
    let mut argv = vec![(*program).to_owned()];
    if let Some(user) = user {
        argv.extend(["-l".to_owned(), user]);
    }
    argv.push(host.clone());
    if let Some(port) = port {
        argv.push(port.to_string());
    }
    Ok(OpenRequest {
        argv,
        cwd: None,
        title: format!("telnet {host}"),
    })
}

/// `x-man-page://[section/]page`, as Terminal accepts it.
fn man_page(rest: &str) -> Result<OpenRequest, OpenError> {
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let rest = rest.trim_start_matches('/').trim_end_matches('/');
    let mut parts = rest.split('/');
    let (section, page) = match (parts.next(), parts.next(), parts.next()) {
        (Some(page), None, _) => (None, page),
        (Some(section), Some(page), None) => (Some(section), page),
        _ => return Err(OpenError::InvalidPage),
    };
    let page = percent_decode(page).ok_or(OpenError::InvalidPage)?;
    let section = section
        .map(|section| percent_decode(section).ok_or(OpenError::InvalidPage))
        .transpose()?;
    if !valid_page(&page)
        || section
            .as_deref()
            .is_some_and(|section| !valid_section(section))
    {
        return Err(OpenError::InvalidPage);
    }
    let mut argv = vec![MAN.to_owned()];
    argv.extend(section.clone());
    argv.push(page.clone());
    let title = match section {
        Some(section) => format!("man {section} {page}"),
        None => format!("man {page}"),
    };
    Ok(OpenRequest {
        argv,
        cwd: None,
        title,
    })
}

fn file(link: &str, system: &impl SystemFacts) -> Result<OpenRequest, OpenError> {
    let url = Url::parse(link).map_err(|_| OpenError::Unsupported)?;
    let path = url.to_file_path().map_err(|()| OpenError::Unsupported)?;
    let runnable = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            RUNNABLE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
    if !runnable || !path.is_absolute() {
        return Err(OpenError::Unsupported);
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let facts = system
        .file(&path)
        .filter(|facts| facts.regular)
        .ok_or_else(|| OpenError::NotFound(name.clone()))?;
    if facts.quarantined {
        return Err(OpenError::Quarantined(name));
    }
    if !facts.executable {
        return Err(OpenError::NotExecutable(name));
    }
    let script = path.to_str().ok_or(OpenError::Unsupported)?.to_owned();
    Ok(OpenRequest {
        argv: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            RUN_THEN_SHELL.to_owned(),
            script,
        ],
        cwd: path.parent().map(Path::to_path_buf),
        title: name,
    })
}

/// Strict `%XX` decoding: a malformed escape or non-UTF-8 result is refused
/// rather than guessed at.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// A DNS name, an `ssh_config` alias, or a dotted address. No option
/// prefixes, `%` tokens, spaces, or shell metacharacters.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= MAX_HOST_BYTES
        && !host.starts_with(['-', '.'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Login names, including `first.last@corp` and `DOMAIN\user` forms.
fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= MAX_USER_BYTES
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '\\'))
}

/// `ls`, `git-commit`, `c++`, `std::vector`, `perl5.30`.
fn valid_page(page: &str) -> bool {
    !page.is_empty()
        && page.len() <= MAX_PAGE_BYTES
        && !page.starts_with(['-', '.'])
        && page
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':'))
}

/// `1`, `3p`, `n`.
fn valid_section(section: &str) -> bool {
    (1..=8).contains(&section.len())
        && section.starts_with(|c: char| c.is_ascii_digit() || c == 'n' || c == 'l')
        && section.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        files: HashMap<PathBuf, FileFacts>,
        programs: Vec<PathBuf>,
    }

    impl SystemFacts for Fake {
        fn file(&self, path: &Path) -> Option<FileFacts> {
            self.files.get(path).copied()
        }

        fn is_executable_program(&self, path: &Path) -> bool {
            self.programs.iter().any(|program| program == path)
        }
    }

    const RUNNABLE: FileFacts = FileFacts {
        regular: true,
        executable: true,
        quarantined: false,
    };

    fn open(link: &str) -> Result<OpenRequest, OpenError> {
        request_for(link, &Fake::default())
    }

    fn argv(link: &str) -> Vec<String> {
        open(link)
            .unwrap_or_else(|error| panic!("{link}: {error:?}"))
            .argv
    }

    #[test]
    fn ssh_links_become_structured_ssh_argv() {
        assert_eq!(argv("ssh://forge"), [SSH, "--", "forge"]);
        assert_eq!(
            argv("ssh://cristi@forge.example.com:2222/"),
            [SSH, "-l", "cristi", "-p", "2222", "--", "forge.example.com"]
        );
        // Host case is kept: `ssh_config` aliases are case-sensitive.
        assert_eq!(argv("SSH://Forge"), [SSH, "--", "Forge"]);
        assert_eq!(argv("ssh://10.0.0.7"), [SSH, "--", "10.0.0.7"]);
        assert_eq!(
            argv("ssh://root@[::1]:22"),
            [SSH, "-l", "root", "-p", "22", "--", "::1"]
        );
        assert_eq!(
            argv("ssh://first.last%40corp@host"),
            [SSH, "-l", "first.last@corp", "--", "host"]
        );
        // SSH URI connection parameters and passwords are never passed on.
        assert_eq!(
            argv("ssh://me;fingerprint=ssh-rsa-aa@host"),
            [SSH, "-l", "me", "--", "host"]
        );
        assert_eq!(
            argv("ssh://me:hunter2@host"),
            [SSH, "-l", "me", "--", "host"]
        );
        let request = open("ssh://me@host").unwrap();
        assert_eq!(request.title, "ssh me@host");
        assert_eq!(request.cwd, None);
    }

    #[test]
    fn hostile_ssh_links_are_refused() {
        for link in [
            "ssh://-oProxyCommand=open%20-a%20Calculator",
            "ssh://-oProxyCommand=touch%20pwned/",
            "ssh://%2DoProxyCommand=x",
            "ssh://-p22",
            "ssh://host%20-oProxyCommand=x",
            "ssh://host;touch%20pwned",
            "ssh://ho$(id)st",
            "ssh://host%25h",
            "ssh://.hidden",
            "ssh://",
            "ssh:host",
            "ssh://user@",
        ] {
            assert_eq!(open(link), Err(OpenError::InvalidHost), "{link}");
        }
        for link in [
            "ssh://-oProxyCommand=x@host",
            "ssh://%2Dl@host",
            "ssh://a%20b@host",
            "ssh://$(id)@host",
            "ssh://%60id%60@host",
            "ssh://%0Aid@host",
            "ssh://%ZZ@host",
            "ssh://%FF@host",
        ] {
            assert_eq!(open(link), Err(OpenError::InvalidUser), "{link}");
        }
        assert_eq!(open("ssh://host:0"), Err(OpenError::InvalidPort));
        assert_eq!(open("ssh://host:99999"), Err(OpenError::InvalidPort));
        assert_eq!(open("ssh://host:-1"), Err(OpenError::InvalidPort));
        let long = format!("ssh://{}", "a".repeat(MAX_LINK_BYTES));
        assert_eq!(open(&long), Err(OpenError::TooLong));
        let long_host = format!("ssh://{}", "a".repeat(MAX_HOST_BYTES + 1));
        assert_eq!(open(&long_host), Err(OpenError::InvalidHost));
    }

    #[test]
    fn every_argument_after_the_double_dash_is_the_validated_host() {
        // Whatever the link, ssh never sees an option where a host belongs.
        for link in ["ssh://a", "ssh://u@a:1", "ssh://[fe80::1]"] {
            let argv = argv(link);
            let dash = argv.iter().position(|argument| argument == "--").unwrap();
            assert_eq!(dash, argv.len() - 2, "{argv:?}");
            assert!(!argv[dash + 1].starts_with('-'));
        }
    }

    #[test]
    fn telnet_links_need_an_installed_telnet() {
        assert_eq!(
            open("telnet://towel.blinkenlights.nl"),
            Err(OpenError::TelnetMissing)
        );
        let system = Fake {
            programs: vec![PathBuf::from("/opt/homebrew/bin/telnet")],
            ..Fake::default()
        };
        assert_eq!(
            request_for("telnet://guest@bbs.example:2323", &system)
                .unwrap()
                .argv,
            [
                "/opt/homebrew/bin/telnet",
                "-l",
                "guest",
                "bbs.example",
                "2323"
            ]
        );
        assert_eq!(
            request_for("telnet://-a", &system),
            Err(OpenError::InvalidHost)
        );
        assert_eq!(
            request_for("telnet://-x@host", &system),
            Err(OpenError::InvalidUser)
        );
    }

    #[test]
    fn man_page_links_run_man() {
        assert_eq!(argv("x-man-page://ls"), [MAN, "ls"]);
        assert_eq!(argv("x-man-page://3/printf"), [MAN, "3", "printf"]);
        assert_eq!(argv("x-man-page:///ls/"), [MAN, "ls"]);
        assert_eq!(argv("x-man-page://git-commit"), [MAN, "git-commit"]);
        assert_eq!(
            argv("x-man-page://3p/std::vector"),
            [MAN, "3p", "std::vector"]
        );
        assert_eq!(argv("x-man-page://c%2B%2B"), [MAN, "c++"]);
        assert_eq!(argv("X-MAN-PAGE://ls?x#y"), [MAN, "ls"]);
        assert_eq!(open("x-man-page://3/printf").unwrap().title, "man 3 printf");
    }

    #[test]
    fn hostile_man_page_links_are_refused() {
        for link in [
            "x-man-page://-P%20open%20-a%20Calculator",
            "x-man-page://%2DH",
            "x-man-page://-k",
            "x-man-page://1/-k",
            "x-man-page://-1/ls",
            "x-man-page://..",
            "x-man-page://1/../ls",
            "x-man-page://a/b/c",
            "x-man-page://ls;id",
            "x-man-page://$(id)",
            "x-man-page://ls%20-P",
            "x-man-page://",
            "x-man-page://toolongsection/ls",
            "x-man-page://%E2%80%8Bls",
        ] {
            assert_eq!(open(link), Err(OpenError::InvalidPage), "{link}");
        }
    }

    #[test]
    fn opened_scripts_run_by_path_in_their_folder_then_leave_a_shell() {
        let path = PathBuf::from("/Users/me/Tools/deploy it; rm -rf ~.command");
        let system = Fake {
            files: HashMap::from([(path.clone(), RUNNABLE)]),
            ..Fake::default()
        };
        let link = Url::from_file_path(&path).unwrap().to_string();
        let request = request_for(&link, &system).unwrap();
        assert_eq!(
            request.argv,
            [
                "/bin/sh",
                "-c",
                RUN_THEN_SHELL,
                "/Users/me/Tools/deploy it; rm -rf ~.command"
            ]
        );
        // The fixed script only ever refers to the path as `$0`.
        assert!(!RUN_THEN_SHELL.contains("deploy"));
        assert_eq!(request.cwd.as_deref(), Some(Path::new("/Users/me/Tools")));
        assert_eq!(request.title, "deploy it; rm -rf ~.command");
    }

    #[test]
    fn only_runnable_local_files_are_opened() {
        let ok = PathBuf::from("/tmp/a.TOOL");
        let sh = PathBuf::from("/tmp/b.sh");
        let plain = PathBuf::from("/tmp/c.command");
        let downloaded = PathBuf::from("/tmp/d.command");
        let folder = PathBuf::from("/tmp/e.command");
        let system = Fake {
            files: HashMap::from([
                (ok.clone(), RUNNABLE),
                (sh.clone(), RUNNABLE),
                (
                    plain.clone(),
                    FileFacts {
                        executable: false,
                        ..RUNNABLE
                    },
                ),
                (
                    downloaded.clone(),
                    FileFacts {
                        quarantined: true,
                        ..RUNNABLE
                    },
                ),
                (
                    folder.clone(),
                    FileFacts {
                        regular: false,
                        ..RUNNABLE
                    },
                ),
            ]),
            ..Fake::default()
        };
        let open = |path: &str| request_for(path, &system);
        assert!(open("file:///tmp/a.TOOL").is_ok());
        assert!(open("file:///tmp/b.sh").is_ok());
        assert_eq!(
            open("file:///tmp/c.command"),
            Err(OpenError::NotExecutable("c.command".into()))
        );
        assert_eq!(
            open("file:///tmp/d.command"),
            Err(OpenError::Quarantined("d.command".into()))
        );
        assert_eq!(
            open("file:///tmp/e.command"),
            Err(OpenError::NotFound("e.command".into()))
        );
        assert_eq!(
            open("file:///tmp/missing.command"),
            Err(OpenError::NotFound("missing.command".into()))
        );
        assert_eq!(open("file:///tmp/notes.txt"), Err(OpenError::Unsupported));
        assert_eq!(
            open("file://otherhost/tmp/a.TOOL"),
            Err(OpenError::Unsupported)
        );
    }

    #[test]
    fn other_links_are_not_ours() {
        for link in [
            "https://example.com",
            "mailto:a@b",
            "javascript:alert(1)",
            "",
            "ssh",
        ] {
            assert_eq!(open(link), Err(OpenError::Unsupported), "{link}");
        }
    }

    /// The wrapper really runs: a hostile file name is only ever `$0`, the
    /// exit code is reported, and the user lands in `$SHELL -l`.
    #[cfg(unix)]
    #[test]
    fn the_run_then_shell_wrapper_never_evaluates_the_file_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir
            .path()
            .join("x; touch PWNED $(touch PWNED2) `touch PWNED3`.command");
        std::fs::write(&script, "#!/bin/sh\necho ran\nexit 3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let system = Fake {
            files: HashMap::from([(script.clone(), RUNNABLE)]),
            ..Fake::default()
        };
        let link = Url::from_file_path(&script).unwrap().to_string();
        let request = request_for(&link, &system).unwrap();
        let output = std::process::Command::new(&request.argv[0])
            .args(&request.argv[1..])
            .current_dir(request.cwd.unwrap())
            .env("SHELL", "/bin/echo")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout, "ran\n\n[Process completed with exit code 3]\n-l\n");
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "nothing but the script exists");
    }

    #[test]
    fn the_local_system_reads_real_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hello.command");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        let facts = LocalSystem.file(&script).unwrap();
        assert!(facts.regular && !facts.executable && !facts.quarantined);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(LocalSystem.file(&script).unwrap().executable);
        }
        assert!(LocalSystem.file(&dir.path().join("missing")).is_none());
    }
}
