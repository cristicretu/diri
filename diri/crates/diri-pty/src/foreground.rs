//! What a terminal's foreground job is, in the words a user would use.
//!
//! A shell session's tab is named after the program in its foreground, and an
//! Agent started by hand inside it is recognised by that same name. Both read
//! the argument vector of the foreground group's leader, which is also where
//! passwords and tokens typed on a command line live, so nothing past the
//! program's own name ever leaves this module.

use std::collections::BTreeMap;
use std::io;

use diri_proto::PortInfo;

/// The program name of `pid`: `vim`, `claude`, `npm`, never its arguments.
///
/// Scripts are named after the script rather than the interpreter running
/// them, so `node /opt/homebrew/bin/codex` is `codex` and `python3 -m http.server`
/// stays `python3`.
pub fn program_name(pid: u32) -> io::Result<String> {
    let (first, second) = leading_arguments(pid)?;
    name_from_arguments(&first, second.as_deref())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unnamed foreground program"))
}

/// The npm package `pid`'s program is installed from: `codebuff` for
/// `node ~/.npm-global/bin/cb`, `@scope/cli` for a native binary an npm
/// package ships. `None` for a program outside any `node_modules` tree.
///
/// Read from the script an interpreter runs, else the executable itself,
/// with symlinks resolved, since npm's `bin` entries are links into the
/// package. Only the package's name leaves this module.
pub fn npm_package(pid: u32) -> io::Result<Option<String>> {
    let (first, second) = leading_arguments(pid)?;
    let script = second.filter(|script| {
        script.starts_with('/') && interpreted(base_name(&first).trim_start_matches('-'))
    });
    let path = match script {
        Some(script) => std::path::PathBuf::from(script),
        None => executable_path(pid)?,
    };
    let path = std::fs::canonicalize(path)?;
    Ok(path.to_str().and_then(package_from_path))
}

/// The package a path inside a `node_modules` tree belongs to: the innermost
/// one, so a CLI's bundled dependency names the dependency.
fn package_from_path(path: &str) -> Option<String> {
    const MODULES: &str = "/node_modules/";
    let rest = &path[path.rfind(MODULES)? + MODULES.len()..];
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let first = parts.next()?;
    let name = if first.starts_with('@') {
        format!("{first}/{}", parts.next()?)
    } else {
        first.to_owned()
    };
    // A path segment may hold anything; a package name holds none of this.
    (name.len() <= 214 && !name.chars().any(|c| c.is_control() || c.is_whitespace()))
        .then_some(name)
}

#[cfg(target_os = "macos")]
fn executable_path(pid: u32) -> io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer is writable for the length passed.
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(length as usize);
    Ok(std::ffi::OsString::from_vec(buffer).into())
}

#[cfg(target_os = "linux")]
fn executable_path(pid: u32) -> io::Result<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn executable_path(_: u32) -> io::Result<std::path::PathBuf> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The live working directory of `pid`, as the kernel reports it.
pub fn working_directory(pid: u32) -> io::Result<String> {
    crate::process_facts::working_directory(pid)
}

/// The TCP ports the processes of group `pgid` listen on, lowest first, each
/// with the name of the process holding it (`node`), never its arguments.
///
/// A dev server's workers share its job's process group, so `npm run dev` is
/// answered by the `node` it started. Read natively rather than through
/// `lsof`: this is asked while a job runs, and a child process per ask would
/// cost more than the answer.
pub fn listening_ports(pgid: u32) -> io::Result<Vec<PortInfo>> {
    let mut ports = BTreeMap::new();
    for pid in sockets::group_members(pgid)? {
        let mut name = None;
        for port in sockets::listening(pid) {
            let name = name.get_or_insert_with(|| sockets::process_name(pid).unwrap_or_default());
            ports.entry(port).or_insert_with(|| name.clone());
        }
    }
    Ok(ports
        .into_iter()
        .map(|(port, process_name)| PortInfo {
            port: i64::from(port),
            process_name,
        })
        .collect())
}

#[cfg(target_os = "macos")]
mod sockets {
    use std::io;

    const PROC_PGRP_ONLY: u32 = 2;
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    // `struct socket_fdinfo` from <sys/proc_info.h>, which libc does not
    // bind. Its layout is kernel ABI; `reads_a_port_this_process_listens_on`
    // fails if these ever stop describing it.
    const SOCKET_FDINFO_SIZE: usize = 792;
    const SOI_KIND: usize = 256;
    const INSI_LPORT: usize = 268;
    const TCPSI_STATE: usize = 344;
    const SOCKINFO_TCP: i32 = 2;
    const TSI_S_LISTEN: i32 = 1;

    pub(super) fn group_members(pgid: u32) -> io::Result<Vec<i32>> {
        // SAFETY: a null buffer asks only for the size needed.
        let bytes = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, pgid, std::ptr::null_mut(), 0) };
        if bytes < 0 {
            return Err(io::Error::last_os_error());
        }
        // Room for members that join between the two calls.
        let mut pids = vec![0i32; bytes as usize / size_of::<i32>() + 16];
        // SAFETY: pids is writable for the byte length passed.
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                pgid,
                pids.as_mut_ptr().cast(),
                (pids.len() * size_of::<i32>()) as libc::c_int,
            )
        };
        if bytes < 0 {
            return Err(io::Error::last_os_error());
        }
        pids.truncate(bytes as usize / size_of::<i32>());
        pids.retain(|pid| *pid > 0);
        Ok(pids)
    }

    pub(super) fn listening(pid: i32) -> Vec<u16> {
        let entry = size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks only for the size needed.
        let bytes =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return Vec::new();
        }
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            bytes as usize / entry + 16
        ];
        // SAFETY: fds is writable for the byte length passed.
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (fds.len() * entry) as libc::c_int,
            )
        };
        if bytes <= 0 {
            return Vec::new();
        }
        fds.truncate(bytes as usize / entry);
        let mut info = [0u8; SOCKET_FDINFO_SIZE];
        let field = |info: &[u8; SOCKET_FDINFO_SIZE], at: usize| {
            i32::from_ne_bytes(info[at..at + 4].try_into().expect("four bytes"))
        };
        fds.iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
            .filter_map(|fd| {
                // SAFETY: info is writable for the byte length passed.
                let read = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        fd.proc_fd,
                        PROC_PIDFDSOCKETINFO,
                        info.as_mut_ptr().cast(),
                        SOCKET_FDINFO_SIZE as libc::c_int,
                    )
                };
                (read as usize == SOCKET_FDINFO_SIZE
                    && field(&info, SOI_KIND) == SOCKINFO_TCP
                    && field(&info, TCPSI_STATE) == TSI_S_LISTEN)
                    // The port is stored in network byte order.
                    .then(|| u16::from_be(field(&info, INSI_LPORT) as u16))
            })
            .filter(|port| *port != 0)
            .collect()
    }

    pub(super) fn process_name(pid: i32) -> Option<String> {
        let mut name = [0u8; 256];
        // SAFETY: name is writable for the byte length passed.
        let length = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        (length > 0).then(|| String::from_utf8_lossy(&name[..length as usize]).into_owned())
    }
}

#[cfg(target_os = "linux")]
mod sockets {
    use std::collections::HashSet;
    use std::io;

    pub(super) fn group_members(pgid: u32) -> io::Result<Vec<i32>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| super::stat_group(&stat))
                == Some(pgid)
            {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    pub(super) fn listening(pid: i32) -> Vec<u16> {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return Vec::new();
        };
        let sockets: HashSet<u64> = fds
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .filter_map(|target| {
                target
                    .to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse()
                    .ok()
            })
            .collect();
        if sockets.is_empty() {
            return Vec::new();
        }
        ["tcp", "tcp6"]
            .iter()
            .filter_map(|table| std::fs::read_to_string(format!("/proc/{pid}/net/{table}")).ok())
            .flat_map(|table| {
                super::listening_sockets(&table)
                    .filter(|(inode, _)| sockets.contains(inode))
                    .map(|(_, port)| port)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub(super) fn process_name(pid: i32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|name| name.trim_end().to_owned())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod sockets {
    pub(super) fn group_members(_: u32) -> std::io::Result<Vec<i32>> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    pub(super) fn listening(_: i32) -> Vec<u16> {
        Vec::new()
    }
    pub(super) fn process_name(_: i32) -> Option<String> {
        None
    }
}

/// The process group in a `/proc/<pid>/stat` line. The command name before it
/// is parenthesised and may itself hold spaces and parentheses.
#[cfg(any(target_os = "linux", test))]
fn stat_group(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // state, ppid, pgrp
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// `(inode, port)` for each listening socket in a `/proc/net/tcp{,6}` table.
#[cfg(any(target_os = "linux", test))]
fn listening_sockets(table: &str) -> impl Iterator<Item = (u64, u16)> + '_ {
    const TCP_LISTEN: &str = "0A";
    table.lines().skip(1).filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (local, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
        if *state != TCP_LISTEN {
            return None;
        }
        let port = u16::from_str_radix(local.rsplit(':').next()?, 16).ok()?;
        Some((inode.parse().ok()?, port))
    })
}

/// Interpreters whose first operand names the program a user started.
const INTERPRETERS: &[&str] = &[
    "node", "nodejs", "bun", "deno", "python", "python2", "python3", "ruby", "perl", "sh", "bash",
    "dash", "zsh", "env",
];

/// Extensions a script's file name carries but its command does not.
const SCRIPT_EXTENSIONS: &[&str] = &[".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".pl", ".sh"];

fn interpreted(program: &str) -> bool {
    INTERPRETERS.contains(&program)
        || program
            .strip_prefix("python")
            .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

fn name_from_arguments(first: &str, second: Option<&str>) -> Option<String> {
    let program = base_name(first).trim_start_matches('-');
    let interpreted = interpreted(program);
    let name = match second {
        Some(script) if interpreted && !script.starts_with('-') && !script.is_empty() => {
            let script = base_name(script);
            SCRIPT_EXTENSIONS
                .iter()
                .find_map(|extension| script.strip_suffix(extension))
                .unwrap_or(script)
        }
        _ => program,
    };
    let name = name.trim();
    (!name.is_empty() && name.len() <= 64 && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Splits a NUL-separated argument block, keeping only the first two entries.
fn first_two(block: &[u8]) -> Option<(String, Option<String>)> {
    let mut parts = block.split(|byte| *byte == 0);
    let first = String::from_utf8(parts.next()?.to_vec()).ok()?;
    if first.is_empty() {
        return None;
    }
    let second = parts
        .next()
        .filter(|part| !part.is_empty())
        .and_then(|part| String::from_utf8(part.to_vec()).ok());
    Some((first, second))
}

#[cfg(target_os = "macos")]
fn leading_arguments(pid: u32) -> io::Result<(String, Option<String>)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = buffer.len();
    // SAFETY: mib is a valid three-level name; buffer is writable for size bytes.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    parse_procargs2(&buffer[..size.min(buffer.len())])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid argument block"))
}

/// `KERN_PROCARGS2` is `argc`, the executable path, NUL padding, then argv.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(bytes: &[u8]) -> Option<(String, Option<String>)> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if argc < 1 {
        return None;
    }
    let rest = &bytes[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let argv_start = path_end + rest[path_end..].iter().position(|byte| *byte != 0)?;
    let (first, second) = first_two(&rest[argv_start..])?;
    Some((first, second.filter(|_| argc >= 2)))
}

#[cfg(target_os = "linux")]
fn leading_arguments(pid: u32) -> io::Result<(String, Option<String>)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(format!("/proc/{pid}/cmdline"))?
        .take(8 * 1024)
        .read_to_end(&mut bytes)?;
    first_two(&bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid argument block"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn leading_arguments(_: u32) -> io::Result<(String, Option<String>)> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The values of `names` in another process's environment (this user's
/// processes only; anyone else's fails). `None` for a name it does not set.
pub fn environment_values(pid: u32, names: &[&str]) -> io::Result<Vec<Option<String>>> {
    let block = environment_block(pid)?;
    Ok(names
        .iter()
        .map(|name| {
            block.iter().find_map(|entry| {
                let value = entry.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
                String::from_utf8(value.to_vec()).ok()
            })
        })
        .collect())
}

#[cfg(target_os = "macos")]
fn environment_block(pid: u32) -> io::Result<Vec<Vec<u8>>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = 0usize;
    // SAFETY: a null buffer asks for the size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: mib is a valid three-level name; buffer is writable for size bytes.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    procargs2_environment(&buffer[..size.min(buffer.len())])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid argument block"))
}

/// The environment entries of a `KERN_PROCARGS2` block: past `argc`, the
/// executable path, its NUL padding and the `argc` arguments.
#[cfg(any(target_os = "macos", test))]
fn procargs2_environment(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    let rest = &bytes[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let start = path_end + rest[path_end..].iter().position(|byte| *byte != 0)?;
    let mut entries = rest[start..].split(|byte| *byte == 0);
    for _ in 0..argc.max(0) {
        entries.next()?;
    }
    Some(
        entries
            .take_while(|entry| !entry.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn environment_block(pid: u32) -> io::Result<Vec<Vec<u8>>> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ"))?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn environment_block(_: u32) -> io::Result<Vec<Vec<u8>>> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child [`the_environment_follows_the_arguments`] reads.
    #[test]
    #[ignore = "a helper process, not a test"]
    fn sleeper() {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    #[test]
    fn the_environment_follows_the_arguments() {
        let mut block = 2i32.to_ne_bytes().to_vec();
        block.extend_from_slice(b"/bin/x\0\0\0x\0-a\0K=v\0DIRIJOR_SOCKET=/s\0\0junk");
        let entries = procargs2_environment(&block).unwrap();
        assert_eq!(
            entries,
            vec![b"K=v".to_vec(), b"DIRIJOR_SOCKET=/s".to_vec()]
        );
        // A real child's environment reads back, unset names as None. Not
        // `/bin/sleep`: macOS withholds Apple platform binaries' environment.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "foreground::tests::sleeper"])
            .env_clear()
            .env("DIRIJOR_SOCKET", "/tmp/probe.sock")
            .spawn()
            .unwrap();
        // Until the child has exec'd, it is still a copy of this process.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let values = loop {
            let values = environment_values(child.id(), &["DIRIJOR_SOCKET", "DIRIJOR_SESSION_ID"]);
            if values.as_ref().is_ok_and(|values| values[0].is_some())
                || std::time::Instant::now() > deadline
            {
                break values;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            values.unwrap(),
            vec![Some("/tmp/probe.sock".to_owned()), None]
        );
    }

    #[test]
    fn programs_are_named_without_their_arguments() {
        assert_eq!(
            name_from_arguments("vim", Some("secret.txt")).unwrap(),
            "vim"
        );
        assert_eq!(name_from_arguments("/usr/bin/htop", None).unwrap(), "htop");
        assert_eq!(name_from_arguments("-zsh", None).unwrap(), "zsh");
        assert_eq!(
            name_from_arguments("claude", Some("--resume")).unwrap(),
            "claude"
        );
    }

    #[test]
    fn a_path_names_the_innermost_npm_package() {
        assert_eq!(
            package_from_path("/opt/homebrew/lib/node_modules/codebuff/index.js").as_deref(),
            Some("codebuff")
        );
        assert_eq!(
            package_from_path(
                "/Users/a/.npm/lib/node_modules/@tencent-ai/codebuddy-code/bin/codebuddy"
            )
            .as_deref(),
            Some("@tencent-ai/codebuddy-code")
        );
        assert_eq!(
            package_from_path("/x/node_modules/@sourcegraph/amp/node_modules/@ampcode/cli/bin/amp")
                .as_deref(),
            Some("@ampcode/cli")
        );
        assert_eq!(package_from_path("/usr/local/bin/cb"), None);
        assert_eq!(package_from_path("/x/node_modules/"), None);
        assert_eq!(package_from_path("/x/node_modules/@scope"), None);
    }

    /// The child's package once it has exec'd (until then it is still a copy
    /// of this process), then the child is stopped.
    fn package_once_execd(mut child: std::process::Child) -> io::Result<Option<String>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let package = loop {
            let package = npm_package(child.id());
            if package.as_ref().is_ok_and(Option::is_some) || std::time::Instant::now() > deadline {
                break package;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = child.kill();
        let _ = child.wait();
        package
    }

    /// A program launched through an npm-style `bin` link is traced back to
    /// the package the link points into.
    #[test]
    fn a_linked_executable_reads_back_its_package() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let package = root.join("lib/node_modules/codebuff");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // A copy, so the executable really lives inside the package and only
        // the link sits on PATH, as npm lays a `bin` entry out.
        let real = package.join("cb");
        std::fs::copy(std::env::current_exe().unwrap(), &real).unwrap();
        let link = root.join("bin/cb");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Another test thread forking while the copy was open for writing
        // inherits that descriptor, and Linux refuses to exec a file open for
        // writing (ETXTBSY) until that child execs or exits.
        let mut attempts = 0;
        let child = loop {
            match std::process::Command::new(&link)
                .args(["--ignored", "--exact", "foreground::tests::sleeper"])
                .spawn()
            {
                Err(error)
                    if error.kind() == io::ErrorKind::ExecutableFileBusy && attempts < 100 =>
                {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => break result.unwrap(),
            }
        };
        let package = package_once_execd(child);
        assert_eq!(package.unwrap().as_deref(), Some("codebuff"));

        // An interpreted entry point is traced through the script it runs,
        // as `#!/usr/bin/env node` leaves `node <bin link>` in argv.
        let script = root.join("lib/node_modules/command-code/dist/index.sh");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "sleep 5\n").unwrap();
        let link = root.join("bin/cmd");
        std::os::unix::fs::symlink(&script, &link).unwrap();
        let child = std::process::Command::new("/bin/sh")
            .arg(&link)
            .spawn()
            .unwrap();
        let package = package_once_execd(child);
        assert_eq!(package.unwrap().as_deref(), Some("command-code"));
    }

    #[test]
    fn scripts_are_named_after_the_script_not_the_interpreter() {
        assert_eq!(
            name_from_arguments("node", Some("/opt/homebrew/bin/codex")).unwrap(),
            "codex"
        );
        assert_eq!(
            name_from_arguments("/usr/bin/python3.12", Some("/tmp/serve.py")).unwrap(),
            "serve"
        );
        assert_eq!(
            name_from_arguments("bash", Some("./cursor-agent")).unwrap(),
            "cursor-agent"
        );
        assert_eq!(
            name_from_arguments("python3", Some("-m")).unwrap(),
            "python3"
        );
        assert_eq!(name_from_arguments("node", None).unwrap(), "node");
    }

    #[test]
    fn procargs2_skips_the_executable_path_and_padding() {
        let mut block = 3i32.to_ne_bytes().to_vec();
        block.extend_from_slice(b"/opt/homebrew/Cellar/node/bin/node\0\0\0\0");
        block.extend_from_slice(b"node\0/opt/homebrew/bin/codex\0--yolo\0HOME=/x\0");
        assert_eq!(
            parse_procargs2(&block).unwrap(),
            ("node".into(), Some("/opt/homebrew/bin/codex".into()))
        );
        let mut single = 1i32.to_ne_bytes().to_vec();
        single.extend_from_slice(b"/bin/vim\0vim\0HOME=/x\0");
        assert_eq!(parse_procargs2(&single).unwrap(), ("vim".into(), None));
    }

    #[test]
    fn reads_a_port_this_process_listens_on() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = i64::from(listener.local_addr().unwrap().port());
        // SAFETY: getpgrp cannot fail.
        let group = unsafe { libc::getpgrp() } as u32;
        let ports = listening_ports(group).unwrap();
        let found = ports.iter().find(|info| info.port == port);
        assert!(
            found.is_some_and(|info| !info.process_name.is_empty()),
            "{ports:?}"
        );
        drop(listener);
        // A sibling test's fork holds a copy of the descriptor until its exec,
        // so the port can outlive the drop briefly; it must then go away.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ports = listening_ports(group).unwrap();
            if ports.iter().all(|info| info.port != port) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{ports:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn proc_tables_yield_listening_ports_and_groups() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:1F90 0100007F:D2A4 01 00000000:00000000 00:00000000 00000000  1000        0 41235 1 0000000000000000 20 4 30 10 -1\n";
        assert_eq!(
            listening_sockets(table).collect::<Vec<_>>(),
            vec![(41234, 3000)]
        );
        assert_eq!(
            stat_group("812 (node (vite)) S 800 790 790 34817"),
            Some(790)
        );
        assert_eq!(stat_group("garbage"), None);
    }

    #[test]
    fn reads_this_process_name() {
        let name = program_name(std::process::id()).unwrap();
        assert!(!name.is_empty() && !name.contains('/'), "{name}");
    }
}
