//! Remote-host support methods: `host.sync_prefs` and `host.locate_repo`.
//!
//! Ported from `PrefsSync` and `RepoLocator`. Every remote command runs over
//! non-interactive, bounded ssh (`BatchMode`, connect timeout, accept-new)
//! so a wedged host can never hang the daemon.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use diri_proto::{HostEntry, HostSyncPrefsResult, PrefsSyncToolReport};

pub const SSH_OPTIONS: [&str; 6] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

/// Single-quotes one value for a fixed POSIX-shell command.
///
/// Agent argv never passes through this helper. It exists only for the
/// bounded host-maintenance commands in this module and `migrate`.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Quotes a path while keeping a leading `~` expandable by the remote shell.
pub(crate) fn shell_quote_path(path: &str) -> String {
    if path == "~" {
        return path.to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", shell_quote(rest)),
        None => shell_quote(path),
    }
}

pub struct ShellOutput {
    pub ok: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `command` on the host (or locally when `host` is None) with a hard
/// timeout; the process is killed at the deadline.
pub fn run_shell(
    host: Option<&HostEntry>,
    command: &str,
    timeout: Duration,
) -> Option<ShellOutput> {
    let mut argv: Vec<String> = match host {
        Some(host) => {
            let mut argv = vec!["ssh".to_string()];
            argv.extend(SSH_OPTIONS.iter().map(ToString::to_string));
            argv.push(host.ssh.clone());
            argv.push("--".into());
            argv.push(command.to_string());
            argv
        }
        None => vec!["/bin/sh".into(), "-c".into(), command.to_string()],
    };
    let program = argv.remove(0);
    run_argv(&program, &argv, timeout)
}

fn run_argv(program: &str, args: &[String], timeout: Duration) -> Option<ShellOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    };
    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Some(ShellOutput {
        ok: status.success(),
        exit_code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

// MARK: Prefs sync

struct ToolSpec {
    name: &'static str,
    local_dir: std::path::PathBuf,
    remote_dir: &'static str,
    items: &'static [&'static str],
}

/// The include list is FIXED and additive (`rsync -a`, never `--delete`).
/// Excluded on purpose: credentials (each user logs in with their own account
/// on the box), transcripts/memory (per-machine, path-slugged), caches.
fn tool_specs(home: &Path) -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "claude",
            local_dir: home.join(".claude"),
            remote_dir: ".claude",
            items: &[
                "CLAUDE.md",
                "settings.json",
                "keybindings.json",
                "commands",
                "skills",
                "agents",
            ],
        },
        ToolSpec {
            name: "codex",
            local_dir: home.join(".codex"),
            remote_dir: ".codex",
            items: &["config.toml", "AGENTS.md", "prompts"],
        },
    ]
}

/// Pushes the local user's agent preferences to a remote host so agents
/// there behave like local ones. Never fails as a whole — each tool reports
/// its own outcome.
pub fn sync_prefs(host: &HostEntry, home: &Path) -> HostSyncPrefsResult {
    let tools = tool_specs(home)
        .into_iter()
        .map(|spec| sync_tool(&spec, host))
        .collect();
    HostSyncPrefsResult { tools }
}

fn sync_tool(spec: &ToolSpec, host: &HostEntry) -> PrefsSyncToolReport {
    let present: Vec<String> = spec
        .items
        .iter()
        .filter(|item| spec.local_dir.join(item).exists())
        .map(ToString::to_string)
        .collect();
    if present.is_empty() {
        // Nothing local to push is a success, not an error.
        return PrefsSyncToolReport {
            tool: spec.name.into(),
            ok: true,
            synced: Vec::new(),
            error: None,
        };
    }

    let failure = |error: String| PrefsSyncToolReport {
        tool: spec.name.into(),
        ok: false,
        synced: Vec::new(),
        error: Some(error),
    };
    match run_shell(
        Some(host),
        &format!("mkdir -p {}", shell_quote(spec.remote_dir)),
        Duration::from_secs(30),
    ) {
        Some(result) if result.ok => {}
        Some(result) => {
            let detail = result.stderr.trim();
            return failure(format!(
                "ssh to {} failed: {}",
                host.display_name(),
                if detail.is_empty() {
                    format!("exit {}", result.exit_code)
                } else {
                    detail.to_string()
                }
            ));
        }
        None => return failure(format!("ssh to {} timed out", host.display_name())),
    }

    let transport = std::iter::once("ssh")
        .chain(SSH_OPTIONS)
        .collect::<Vec<_>>()
        .join(" ");
    let mut args: Vec<String> = vec!["-a".into(), "--timeout=60".into(), "-e".into(), transport];
    args.extend(
        present
            .iter()
            .map(|item| spec.local_dir.join(item).to_string_lossy().into_owned()),
    );
    args.push(format!("{}:{}/", host.ssh, spec.remote_dir));
    match run_argv("rsync", &args, Duration::from_secs(120)) {
        Some(result) if result.ok => PrefsSyncToolReport {
            tool: spec.name.into(),
            ok: true,
            synced: present,
            error: None,
        },
        Some(result) => failure(rsync_failure_message(&result, host)),
        None => failure(format!("rsync to {} timed out", host.display_name())),
    }
}

/// The classic trap is a remote box without rsync installed: the remote shell
/// prints "command not found" and rsync dies with a protocol error — say so
/// plainly.
fn rsync_failure_message(result: &ShellOutput, host: &HostEntry) -> String {
    let stderr = result.stderr.trim();
    let lowered = stderr.to_lowercase();
    if lowered.contains("command not found")
        || lowered.contains("rsync: not found")
        || result.exit_code == 127
    {
        return format!(
            "rsync is not installed on {} — install it there (e.g. apt install rsync) and retry",
            host.display_name()
        );
    }
    format!("rsync failed (exit {}): {stderr}", result.exit_code)
}

// MARK: Repo location

/// Canonicalizes an origin URL so the ssh and https spellings of one repo
/// compare equal: `git@github.com:org/x.git` == `https://github.com/org/x`.
pub fn normalize_git_url(url: &str) -> String {
    let mut s = url.trim().to_string();
    let mut had_scheme = false;
    for prefix in ["ssh://", "git://", "https://", "http://", "file://"] {
        if s.to_lowercase().starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            had_scheme = true;
        }
    }
    // scp-like syntax (only without a scheme — after one, `host:2222` is a
    // port): [user@]host:path → host/path.
    if !had_scheme
        && let Some(colon) = s.find(':')
        && !s[..colon].contains('/')
    {
        s = format!("{}/{}", &s[..colon], &s[colon + 1..]);
    }
    // Drop user@ and an explicit :port from the host component.
    if let Some(at) = s.find('@')
        && !s[..at].contains('/')
    {
        s = s[at + 1..].to_string();
    }
    if let Some(slash) = s.find('/')
        && slash > 0
    {
        let mut host_part = s[..slash].to_string();
        if let Some(port_colon) = host_part.find(':') {
            host_part.truncate(port_colon);
        }
        s = format!("{host_part}{}", &s[slash..]);
    }
    while s.ends_with('/') {
        s.pop();
    }
    if s.to_lowercase().ends_with(".git") {
        s.truncate(s.len() - 4);
    }
    s.to_lowercase()
}

/// The origin URL of the repository containing `cwd` on `host` (None host =
/// local). None when cwd isn't in a git repo or the repo has no origin.
pub fn origin_of_cwd(cwd: &str, host: Option<&HostEntry>) -> Option<String> {
    let command = format!("cd {} && git remote get-url origin", shell_quote_path(cwd));
    let result = run_shell(host, &command, Duration::from_secs(20))?;
    if !result.ok {
        return None;
    }
    let origin = result.stdout.trim();
    (!origin.is_empty()).then(|| origin.to_string())
}

/// One `path<TAB>origin` line per repo directly under the host's defaultCwd.
/// `cd && pwd` yields the ABSOLUTE path even when defaultCwd is `~`-relative.
pub fn remote_list_command(default_cwd: &str) -> String {
    let root = shell_quote_path(default_cwd);
    format!(
        "for d in {root}/*/; do \
         [ -e \"$d/.git\" ] || continue; \
         printf '%s\\t%s\\n' \"$(cd \"$d\" && pwd)\" \
         \"$(git -C \"$d\" remote get-url origin 2>/dev/null)\"; \
         done"
    )
}

pub fn parse_repo_list(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (path, origin) = line.split_once('\t')?;
            (!origin.is_empty()).then(|| (path.to_string(), origin.to_string()))
        })
        .collect()
}

/// Absolute path of a checkout with the given origin, or None. A remote
/// search scans one directory level under the host's `defaultCwd` (the
/// documented layout for remote clones); a local search checks the given
/// project roots.
pub fn locate(origin: &str, host: Option<&HostEntry>, local_roots: &[String]) -> Option<String> {
    let normalized = normalize_git_url(origin);
    match host {
        Some(host) => {
            let result = run_shell(
                Some(host),
                &remote_list_command(host.default_cwd.as_deref().unwrap_or("~")),
                Duration::from_secs(20),
            )?;
            if !result.ok {
                return None;
            }
            parse_repo_list(&result.stdout)
                .into_iter()
                .find(|(_, candidate)| normalize_git_url(candidate) == normalized)
                .map(|(path, _)| path)
        }
        None => local_roots.iter().find_map(|root| {
            if !Path::new(root).exists() {
                return None;
            }
            let result = run_shell(
                None,
                &format!("git -C {} remote get-url origin", shell_quote(root)),
                Duration::from_secs(10),
            )?;
            (result.ok && normalize_git_url(result.stdout.trim()) == normalized)
                .then(|| root.clone())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_urls_normalize_across_spellings() {
        for (a, b) in [
            ("git@github.com:Org/X.git", "https://github.com/org/x"),
            ("ssh://git@github.com/org/x.git", "github.com/org/x/"),
            ("https://user@github.com:443/org/x", "github.com/org/x"),
        ] {
            assert_eq!(normalize_git_url(a), normalize_git_url(b), "{a} vs {b}");
        }
        assert_ne!(
            normalize_git_url("github.com/org/x"),
            normalize_git_url("github.com/org/y")
        );
    }

    #[test]
    fn the_repo_list_parses_paths_and_origins() {
        let output = "/home/u/code/app\tgit@github.com:o/app.git\n/home/u/code/junk\t\n";
        let repos = parse_repo_list(output);
        assert_eq!(repos.len(), 1, "originless dirs are skipped");
        assert_eq!(repos[0].0, "/home/u/code/app");
    }

    #[test]
    fn local_origin_lookup_finds_a_real_repo() {
        // This crate's own checkout has an origin remote.
        let cwd = env!("CARGO_MANIFEST_DIR");
        let origin = origin_of_cwd(cwd, None);
        assert!(origin.is_some(), "the dev checkout has an origin");
        let root = crate::git::repository_root(Path::new(cwd)).expect("in a repo");
        let located = locate(&origin.unwrap(), None, std::slice::from_ref(&root));
        assert_eq!(located, Some(root));
    }

    #[test]
    fn prefs_sync_reports_per_tool_without_a_host_roundtrip() {
        // A tool with nothing local to push reports ok with an empty list.
        let empty = tempfile::tempdir().expect("temp");
        let result = sync_prefs(
            &HostEntry {
                id: "x".into(),
                name: None,
                ssh: "nonexistent.invalid".into(),
                default_cwd: None,
                node: None,
            },
            empty.path(),
        );
        assert_eq!(result.tools.len(), 2);
        assert!(
            result
                .tools
                .iter()
                .all(|tool| tool.ok && tool.synced.is_empty())
        );
    }

    /// Quoting the whole path would send a literal tilde and land the session
    /// in a directory named `~`, so the prefix is deliberately left bare.
    #[test]
    fn a_tilde_path_stays_expandable() {
        assert_eq!(shell_quote_path("~/code/app"), "~/'code/app'");
        assert_eq!(shell_quote_path("~"), "~");
        assert_eq!(shell_quote_path("/abs/path"), "'/abs/path'");
    }

    /// These two functions build every remote shell command the Engine sends,
    /// so this is the injection guard. It travelled here from the deleted
    /// `remote` module and would otherwise have been lost with it — `hosts`
    /// shipped no coverage for either.
    #[test]
    fn a_path_with_a_quote_cannot_break_out() {
        let quoted = shell_quote_path("~/it's here");
        assert_eq!(quoted, r"~/'it'\''s here'");

        // What a shell actually sees: '…' + escaped quote + '…' → one word.
        let echoed = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("printf %s {quoted}")])
            .env("HOME", "/tmp")
            .output()
            .expect("sh");
        assert_eq!(
            String::from_utf8_lossy(&echoed.stdout),
            "/tmp/it's here",
            "the quoting must round-trip through a real shell"
        );
    }

    #[test]
    fn a_command_substitution_cannot_escape_shell_quote() {
        let quoted = shell_quote("$(touch /tmp/diri-injection-canary)");
        let echoed = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("printf %s {quoted}")])
            .output()
            .expect("sh");
        assert_eq!(
            String::from_utf8_lossy(&echoed.stdout),
            "$(touch /tmp/diri-injection-canary)",
            "the substitution must arrive as literal text, not run"
        );
        assert!(
            !std::path::Path::new("/tmp/diri-injection-canary").exists(),
            "the quoted command substitution must not have executed"
        );
    }
}
